//! The connection supervisor: one server's connection generations, and
//! reconnecting with backoff when the connection drops. (dsh:
//! `startConnection` and `resolveReconnectPolicy` in
//! `mcp-client/src/connection.ts`.)
//!
//! Each attempt is a generation: a fresh transport and a fresh [`Client`].
//! Only the current generation may act; a late signal from a replaced one
//! is ignored. A connected generation is handed to the
//! [`ConnectionObserver`] to sync its tools, and every sync, initial or
//! after a `list_changed`, runs one at a time.
//!
//! One outage shares one attempt budget: `max_attempts` consecutive failed
//! attempts, the delay doubling from `initial_delay_ms` up to
//! `max_delay_ms`. A connection that stays up past `max_delay_ms` ends the
//! outage, so the next one starts a fresh budget, while a crash loop whose
//! connects briefly succeed still exhausts it. Exhaustion clears the
//! server's tools and stops; only disposing the connection, and starting
//! another, comes back from it.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::client::{Client, ClientOptions};
use crate::http::{HttpParams, HttpTransport};
use crate::protocol::McpError;
use crate::stdio::{StdioParams, StdioTransport};
use crate::transport::Transport;

/// The largest delay a JavaScript timer takes. (dsh: `MAX_TIMER_DELAY_MS`.)
pub const MAX_TIMER_DELAY_MS: f64 = 2_147_483_647.0;

/// The UTF-8 bytes a server's attributed instructions may take. (dsh:
/// `DEFAULT_MAX_INSTRUCTION_BYTES`.)
pub const DEFAULT_MAX_INSTRUCTION_BYTES: usize = 32_768;

/// A tool call's or a resource request's default timeout. (dsh:
/// `DEFAULT_TOOL_CALL_TIMEOUT_MS`.)
pub const DEFAULT_TOOL_CALL_TIMEOUT_MS: u64 = 60_000;

/// A resolved reconnect policy. (dsh: `ResolvedReconnectPolicy`.)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReconnectPolicy {
    /// Reconnect after a lost connection.
    pub enabled: bool,
    /// The first delay, in milliseconds; it doubles per failed attempt.
    pub initial_delay_ms: f64,
    /// The delay's ceiling, and the uptime after which the budget resets.
    pub max_delay_ms: f64,
    /// Consecutive failed attempts per outage before giving up.
    pub max_attempts: u64,
}

/// The defaults. (dsh: `RECONNECT_DEFAULTS`.)
pub const RECONNECT_DEFAULTS: ReconnectPolicy = ReconnectPolicy {
    enabled: true,
    initial_delay_ms: 500.0,
    max_delay_ms: 30_000.0,
    max_attempts: 10,
};

/// The policy a raw `reconnect` config resolves to, every default and
/// bound judged here, each failure in dsh's words, prefixed with `path`.
/// `None` is the defaults. (dsh: `resolveReconnectPolicy`.)
///
/// # Errors
///
/// An unknown key, a value of the wrong type, or a value out of range.
pub fn resolve_reconnect_policy(
    config: Option<&Value>,
    path: &str,
) -> Result<ReconnectPolicy, String> {
    const KEYS: [&str; 4] = ["enabled", "initialDelayMs", "maxDelayMs", "maxAttempts"];
    let empty = serde_json::Map::new();
    let config = match config {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(config)) => config,
        Some(_) => return Err(format!("{path} must be an object")),
    };
    if let Some(key) = config.keys().find(|key| !KEYS.contains(&key.as_str())) {
        return Err(format!("{path}.{key} is not a reconnect option"));
    }
    let enabled = match config.get("enabled") {
        None => RECONNECT_DEFAULTS.enabled,
        Some(Value::Bool(enabled)) => *enabled,
        Some(_) => return Err(format!("{path}.enabled must be a boolean")),
    };
    let delay = |key: &str, default: f64| -> Result<f64, String> {
        let wrong = || {
            format!(
                "{path}.{key} must be a positive finite number no greater than {}",
                MAX_TIMER_DELAY_MS as u64
            )
        };
        let value = match config.get(key) {
            None => default,
            Some(value) => value.as_f64().ok_or_else(wrong)?,
        };
        if !value.is_finite() || value <= 0.0 || value > MAX_TIMER_DELAY_MS {
            return Err(wrong());
        }
        Ok(value)
    };
    let initial_delay_ms = delay("initialDelayMs", RECONNECT_DEFAULTS.initial_delay_ms)?;
    let max_delay_ms = delay("maxDelayMs", RECONNECT_DEFAULTS.max_delay_ms)?;
    if initial_delay_ms > max_delay_ms {
        return Err(format!(
            "{path}.initialDelayMs must be less than or equal to maxDelayMs"
        ));
    }
    let max_attempts = match config.get("maxAttempts") {
        None => RECONNECT_DEFAULTS.max_attempts,
        Some(value) => {
            let attempts = value.as_f64().unwrap_or(f64::NAN);
            if attempts.fract() != 0.0 || attempts < 1.0 || !attempts.is_finite() {
                return Err(format!("{path}.maxAttempts must be a positive integer"));
            }
            attempts as u64
        }
    };
    Ok(ReconnectPolicy {
        enabled,
        initial_delay_ms,
        max_delay_ms,
        max_attempts,
    })
}

/// Which transport a server is reached by. (dsh: `Config`'s `transport`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportConfig {
    Stdio(StdioParams),
    StreamableHttp(HttpParams),
}

/// Makes a fresh transport for each attempt. (dsh: `createTransport`.)
pub type TransportFactory = Arc<dyn Fn() -> Box<dyn Transport> + Send + Sync>;

/// The factory for `config`'s transport.
pub fn transport_factory(config: TransportConfig) -> TransportFactory {
    Arc::new(move || -> Box<dyn Transport> {
        match &config {
            TransportConfig::Stdio(params) => Box::new(StdioTransport::new(params.clone())),
            TransportConfig::StreamableHttp(params) => Box::new(HttpTransport::new(params.clone())),
        }
    })
}

/// One server's settings, as the supervisor uses them. (dsh: `Config`'s
/// `serverName`, `toolCallTimeoutMs`, `failOnStartupError` and
/// `maxInstructionBytes`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionConfig {
    pub server_name: String,
    pub tool_call_timeout_ms: u64,
    /// The first attempt's sync is strict: the observer is told it is the
    /// startup attempt, and the plugin fails if it does.
    pub fail_on_startup_error: bool,
    pub max_instruction_bytes: usize,
}

impl ConnectionConfig {
    pub fn new(server_name: impl Into<String>) -> Self {
        Self {
            server_name: server_name.into(),
            tool_call_timeout_ms: DEFAULT_TOOL_CALL_TIMEOUT_MS,
            fail_on_startup_error: false,
            max_instruction_bytes: DEFAULT_MAX_INSTRUCTION_BYTES,
        }
    }
}

/// How bad a supervisor's line is. (dsh: the logger's levels.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// What a sync resolves to.
pub type SyncFuture<'a> = Pin<Box<dyn Future<Output = Result<(), McpError>> + Send + 'a>>;

/// Who owns what a connection publishes: its tools, and its log lines.
/// (dsh: `syncTools`, the disposers it keeps, and `ctx.logger`.)
pub trait ConnectionObserver: Send + Sync {
    /// Publishes `client`'s tools, replacing the previous generation's.
    /// `strict` is the startup attempt's sync with `failOnStartupError`
    /// set, where a registration conflict is an error rather than
    /// contained. The supervisor runs one at a time.
    fn sync<'a>(&'a self, _client: &'a Client, _strict: bool) -> SyncFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    /// Takes back everything published: on giving up, and on disposal.
    fn clear(&self) {}

    /// One line the supervisor reports.
    fn log(&self, _level: Level, _message: &str) {}
}

/// How the first attempt went. (dsh: `ConnectionOutcome`.)
pub type Outcome = Result<(), McpError>;

struct Inner {
    config: ConnectionConfig,
    policy: ReconnectPolicy,
    factory: TransportFactory,
    observer: Arc<dyn ConnectionObserver>,
    label: String,
    state: Mutex<State>,
    /// One sync at a time, and the give-up's clear behind them. (dsh:
    /// `syncChain`.)
    syncing: tokio::sync::Mutex<()>,
    ready: watch::Sender<Option<Outcome>>,
    /// Ends a reconnect wait.
    dispose: watch::Sender<bool>,
    me: Weak<Inner>,
}

#[derive(Default)]
struct State {
    disposed: bool,
    /// The connecting or connected generation.
    current: Option<(u64, Client)>,
    generations: u64,
    failed_attempts: u64,
    connected_at: Option<Instant>,
    first_error: Option<McpError>,
    instructions: String,
    /// The attempt in flight, or the reconnect waiting to start one.
    settling: Option<JoinHandle<()>>,
}

/// One server's supervised connection. (dsh: `ConnectionHandle`.)
#[derive(Clone)]
pub struct ConnectionHandle {
    inner: Arc<Inner>,
}

/// Starts the supervised connection, and its first attempt. Call it in a
/// Tokio runtime. (dsh: `startConnection`.)
pub fn start_connection(
    config: ConnectionConfig,
    policy: ReconnectPolicy,
    factory: TransportFactory,
    observer: Arc<dyn ConnectionObserver>,
) -> ConnectionHandle {
    let label = format!("mcp-client({})", config.server_name);
    let inner = Arc::new_cyclic(|me| Inner {
        config,
        policy,
        factory,
        observer,
        label,
        state: Mutex::new(State::default()),
        syncing: tokio::sync::Mutex::new(()),
        ready: watch::channel(None).0,
        dispose: watch::channel(false).0,
        me: me.clone(),
    });
    let first = Arc::clone(&inner);
    let settling = tokio::spawn(async move {
        first.attempt(true).await;
        let outcome = {
            let state = first.lock();
            if state.current.is_some() {
                Ok(())
            } else {
                Err(state.first_error.clone().unwrap_or_else(|| {
                    McpError::other(format!("{}: initial connection failed", first.label))
                }))
            }
        };
        first.ready.send_replace(Some(outcome));
    });
    inner.lock().settling = Some(settling);
    ConnectionHandle { inner }
}

impl ConnectionHandle {
    /// Resolves once the first attempt has settled, with its error if it
    /// failed. The supervisor retries regardless; the caller decides
    /// whether a failed start is fatal. (dsh: `ready`.)
    pub async fn ready(&self) -> Outcome {
        let mut ready = self.inner.ready.subscribe();
        let outcome = ready.wait_for(Option::is_some).await;
        match outcome {
            Ok(outcome) => outcome.clone().unwrap_or(Ok(())),
            Err(_) => Err(McpError::other(format!("{}: disposed", self.inner.label))),
        }
    }

    /// The connected server's attributed instructions, `### MCP server:
    /// <name>` then its text; empty while none are active. (dsh:
    /// `instructions`.)
    pub fn instructions(&self) -> String {
        self.inner.lock().instructions.clone()
    }

    /// The connected generation, to make requests through.
    ///
    /// # Errors
    ///
    /// `<label>: server is disconnected` while no generation is connected.
    pub fn client(&self) -> Result<Client, McpError> {
        let state = self.inner.lock();
        match (&state.current, state.connected_at) {
            (Some((_, client)), Some(_)) => Ok(client.clone()),
            _ => Err(McpError::other(format!(
                "{}: server is disconnected",
                self.inner.label
            ))),
        }
    }

    /// The connection's settings.
    pub fn config(&self) -> &ConnectionConfig {
        &self.inner.config
    }

    /// Stops reconnecting, closes the connecting or connected generation,
    /// waits for the attempt in flight and the syncs queued to finish, then
    /// clears what was published. (dsh: `dispose`.)
    pub async fn dispose(&self) {
        let inner = &self.inner;
        let (client, settling) = {
            let mut state = inner.lock();
            if state.disposed {
                (None, state.settling.take())
            } else {
                state.disposed = true;
                state.instructions.clear();
                (
                    state.current.take().map(|(_, client)| client),
                    state.settling.take(),
                )
            }
        };
        inner.dispose.send_replace(true);
        if let Some(client) = client
            && !client.shutdown().await
        {
            inner.observer.log(
                Level::Error,
                &format!(
                    "{}: transport closure could not be confirmed during disposal — server shutdown may be incomplete",
                    inner.label
                ),
            );
        }
        if let Some(settling) = settling {
            let _ = settling.await;
        }
        let _syncs = inner.syncing.lock().await;
        inner.observer.clear();
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn log(&self, level: Level, message: &str) {
        self.observer
            .log(level, &format!("{}: {message}", self.label));
    }

    fn is_current(&self, generation: u64) -> bool {
        let state = self.lock();
        !state.disposed
            && state
                .current
                .as_ref()
                .is_some_and(|(id, _)| *id == generation)
    }

    /// One attempt. Never fails: every failure leads to the generation
    /// going down. (dsh: `connectGeneration`.)
    fn attempt(self: &Arc<Self>, startup: bool) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let this = Arc::clone(self);
        Box::pin(async move { this.run_attempt(startup).await })
    }

    async fn run_attempt(self: Arc<Self>, startup: bool) {
        let generation = {
            let mut state = self.lock();
            state.generations += 1;
            state.generations
        };
        let settled = Arc::new(AtomicBool::new(false));
        let options = ClientOptions {
            on_tools_changed: Some({
                let me = self.me.clone();
                Arc::new(move || {
                    if let Some(me) = me.upgrade() {
                        tokio::spawn(me.refresh(generation));
                    }
                })
            }),
            on_close: Some({
                let me = self.me.clone();
                let settled = Arc::clone(&settled);
                Arc::new(move || {
                    // A failed connect owns its close in its error path; an
                    // established generation goes down from this signal.
                    if settled.load(Ordering::SeqCst)
                        && let Some(me) = me.upgrade()
                    {
                        me.down(generation);
                    }
                })
            }),
            ..ClientOptions::default()
        };
        let client = Client::new((self.factory)(), options);
        {
            let mut state = self.lock();
            if state.disposed {
                return;
            }
            state.current = Some((generation, client.clone()));
        }
        let strict = startup && self.config.fail_on_startup_error;
        let connected = async {
            client.connect().await?;
            if client.is_closed() || !self.is_current(generation) {
                return Ok(None);
            }
            let text = client.instructions().unwrap_or_default();
            let text = text.trim_end();
            let instructions = if text.is_empty() {
                String::new()
            } else {
                format!("### MCP server: {}\n\n{text}", self.config.server_name)
            };
            if instructions.len() > self.config.max_instruction_bytes {
                return Err(McpError::other(format!(
                    "{}: server instructions exceed maxInstructionBytes ({})",
                    self.label, self.config.max_instruction_bytes
                )));
            }
            self.sync(generation, &client, strict).await?;
            Ok(Some(instructions))
        };
        let instructions = match connected.await {
            Ok(instructions) => instructions,
            Err(error) => {
                let current = self.is_current(generation);
                self.lock().first_error.get_or_insert_with(|| error.clone());
                if current {
                    self.log(
                        Level::Warn,
                        &format!("connection attempt failed: {}", error.display_with_name()),
                    );
                }
                let quiesced = client.shutdown().await;
                settled.store(true, Ordering::SeqCst);
                self.settle_failed(generation, quiesced);
                return;
            }
        };
        settled.store(true, Ordering::SeqCst);
        if client.is_closed() {
            self.down(generation);
            return;
        }
        if !self.is_current(generation) {
            if !client.shutdown().await {
                self.log(
                    Level::Error,
                    "transport closure could not be confirmed during disposal — server shutdown may be incomplete",
                );
            }
            return;
        }
        let failed_attempts = {
            let mut state = self.lock();
            state.instructions = instructions.unwrap_or_default();
            state.connected_at = Some(Instant::now());
            state.failed_attempts
        };
        if failed_attempts > 0 {
            self.log(
                Level::Info,
                &format!(
                    "reconnected and re-synced tools (attempt {failed_attempts}/{})",
                    self.policy.max_attempts
                ),
            );
        }
    }

    /// Runs one sync for `generation`, if it is still current, after the
    /// syncs before it. (dsh: `enqueueSync`.)
    async fn sync(&self, generation: u64, client: &Client, strict: bool) -> Result<(), McpError> {
        let _one = self.syncing.lock().await;
        if !self.is_current(generation) {
            return Ok(());
        }
        self.observer.sync(client, strict).await
    }

    /// The server's tools changed: sync them again. (dsh: `refreshTools`.)
    async fn refresh(self: Arc<Self>, generation: u64) {
        let client = {
            let state = self.lock();
            match &state.current {
                Some((id, client)) if *id == generation && !state.disposed => client.clone(),
                _ => return,
            }
        };
        self.log(Level::Info, "tool list changed, re-syncing");
        if let Err(error) = self.sync(generation, &client, false).await
            && !self.lock().disposed
        {
            self.log(
                Level::Error,
                &format!("tool re-sync failed: {}", error.display_with_name()),
            );
        }
    }

    /// The generation went down: schedule what comes next, once per
    /// generation. (dsh: `generationDown`.)
    fn down(self: &Arc<Self>, generation: u64) {
        if !self.is_current(generation) {
            return;
        }
        self.lock().current = None;
        self.schedule();
    }

    /// A failed generation's close settled. Without proof that it closed,
    /// no replacement starts, so two server processes never overlap. (dsh:
    /// `settleFailedGeneration`.)
    fn settle_failed(self: &Arc<Self>, generation: u64, quiesced: bool) {
        if !self.is_current(generation) {
            return;
        }
        if !quiesced {
            self.lock().current = None;
            self.log(
                Level::Error,
                "failed generation could not confirm transport closure — reconnect stopped to avoid overlapping server processes; reload the plugin or restart the Host to retry",
            );
            return;
        }
        self.down(generation);
    }

    /// Schedules the next attempt, or gives up. (dsh: `scheduleReconnect`.)
    fn schedule(self: &Arc<Self>) {
        let lost = self.lock().connected_at.is_some();
        if !self.policy.enabled {
            let message = if lost {
                "connection lost and reconnect is disabled — registered tools will fail until an HMR reload or Host restart"
            } else {
                "connection failed and reconnect is disabled — no tools were registered; reload the plugin or restart the Host to connect"
            };
            self.log(Level::Error, message);
            return;
        }
        let attempt = {
            let mut state = self.lock();
            // Up past the stability window ended the previous outage.
            if let Some(at) = state.connected_at
                && at.elapsed() >= Duration::from_secs_f64(self.policy.max_delay_ms / 1000.0)
            {
                state.failed_attempts = 0;
            }
            state.connected_at = None;
            state.failed_attempts += 1;
            state.failed_attempts
        };
        if attempt > self.policy.max_attempts {
            // Behind any sync in flight, so it can't race a publish.
            let this = Arc::clone(self);
            tokio::spawn(async move {
                let _one = this.syncing.lock().await;
                this.observer.clear();
                this.lock().instructions.clear();
            });
            self.log(
                Level::Error,
                &format!(
                    "giving up after {} consecutive failed reconnect attempts — tools unregistered; reload the plugin or restart the Host to reconnect",
                    self.policy.max_attempts
                ),
            );
            return;
        }
        let exponent = i32::try_from(attempt - 1).unwrap_or(i32::MAX);
        let delay_ms =
            (self.policy.initial_delay_ms * 2f64.powi(exponent)).min(self.policy.max_delay_ms);
        let action = if lost {
            "connection lost; reconnecting"
        } else {
            "connection failed; retrying"
        };
        self.log(
            Level::Warn,
            &format!(
                "{action} in {}ms (attempt {attempt}/{})",
                format_ms(delay_ms),
                self.policy.max_attempts
            ),
        );
        let this = Arc::clone(self);
        let mut disposed = self.dispose.subscribe();
        let task = tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs_f64(delay_ms / 1000.0)) => {}
                _ = disposed.wait_for(|disposed| *disposed) => return,
            }
            if this.lock().disposed {
                return;
            }
            this.attempt(false).await;
        });
        let mut state = self.lock();
        if !state.disposed {
            state.settling = Some(task);
        }
    }
}

/// A delay as JavaScript prints a number: no `.0` for a whole one.
fn format_ms(ms: f64) -> String {
    if ms.fract() == 0.0 {
        format!("{}", ms as u64)
    } else {
        ms.to_string()
    }
}
