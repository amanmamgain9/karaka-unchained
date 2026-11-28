//! One connection to one server: version negotiation, the handshake,
//! requests, and what the server sends unasked. (SDK: `Client` and its
//! `Protocol` base, `client/dist/index.mjs` and `src-*.mjs`, with dsh's
//! options: `versionNegotiation: { mode: 'auto' }` and a `tools`
//! `listChanged` handler with `autoRefresh: false`.)
//!
//! # Negotiation
//!
//! The client first asks `server/discover`, the modern era's probe:
//!
//! - **stdio** probes on a disposable twin of the transport, spawned from the
//!   same parameters with its stderr discarded and reaped before the real
//!   one starts, so a server that exits on any request before `initialize`
//!   doesn't cost the connection its one process.
//! - **HTTP**, and any transport with no twin, probes in place, ignoring
//!   everything but the probe's answer.
//!
//! A `server/discover` result offering `2026-07-28` is a modern verdict, and
//! the connection runs that era with no `initialize`. Anything else that
//! reads as "not modern" falls back to the 2025-era handshake, `initialize`
//! then `notifications/initialized`: a JSON-RPC error of any code, a result
//! that doesn't fit, and on stdio a close or a timeout. An HTTP status of
//! 401, 403 or 500 and up, a network error, or on HTTP a close or a timeout,
//! fails the connection with the SDK's message.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use karaka_fs::AbortSignal;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::{oneshot, watch};

use crate::protocol::{
    ErrorKind, LATEST_PROTOCOL_VERSION, MODERN_METHODS, MODERN_PROTOCOL_VERSION, McpError,
    SUPPORTED_PROTOCOL_VERSIONS, client_info, code, envelope, meta,
};
use crate::transport::{Inbound, Transport, TransportKind};

/// How long a supervisor waits for a closed transport to say so. (dsh:
/// `GENERATION_CLOSE_TIMEOUT_MS`, "the SDK's stdio transport owns two
/// two-second termination grace periods", plus one.)
pub const CLOSE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);

/// The id of the modern era's change subscription. (SDK: `listen:0`.)
const LISTEN_ID: &str = "listen:0";

/// A callback the client calls with no arguments.
pub type Callback = Arc<dyn Fn() + Send + Sync>;

/// A callback the client calls with a message.
pub type ErrorCallback = Arc<dyn Fn(String) + Send + Sync>;

/// What the client tells its owner. (SDK: `listChanged.tools.onChanged`,
/// `onerror` and `onclose`.)
#[derive(Clone, Default)]
pub struct ClientOptions {
    /// The server's tool list changed. Only heard when the server declares
    /// `capabilities.tools.listChanged`, as the SDK registers the handler.
    pub on_tools_changed: Option<Callback>,
    /// A failure that doesn't end the connection.
    pub on_error: Option<ErrorCallback>,
    /// The connection closed: once, whoever closed it.
    pub on_close: Option<Callback>,
    /// How long the probe waits; [`DEFAULT_REQUEST_TIMEOUT_MS`] by default.
    ///
    /// [`DEFAULT_REQUEST_TIMEOUT_MS`]: crate::DEFAULT_REQUEST_TIMEOUT_MS
    pub probe_timeout: Option<Duration>,
}

/// The era a connection negotiated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Era {
    /// `initialize` agreed on this 2025-era revision.
    Legacy(String),
    /// `server/discover` agreed on `2026-07-28`.
    Modern,
}

/// One request's limits. (SDK: `RequestOptions`.)
#[derive(Clone, Debug, Default)]
pub struct RequestOptions {
    /// [`DEFAULT_REQUEST_TIMEOUT_MS`](crate::DEFAULT_REQUEST_TIMEOUT_MS)
    /// when `None`.
    pub timeout: Option<Duration>,
    pub cancel: Option<AbortSignal>,
}

/// One page of `tools/list`.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolsPage {
    pub tools: Vec<Value>,
    pub next_cursor: Option<String>,
    /// The whole result, for fields a caller wants beyond these.
    pub result: Value,
}

/// How many pages `list_all_tools` walks. (SDK: `listTools` with no
/// cursor walks up to 64.)
const MAX_TOOL_PAGES: usize = 64;

/// A response, or what ended the wait for one.
enum Answer {
    /// The whole response message.
    Response(Value),
    Closed,
}

/// The transport and everything that came through it.
struct Peer {
    transport: Arc<dyn Transport>,
    pending: Mutex<HashMap<String, oneshot::Sender<Answer>>>,
    closed: watch::Sender<bool>,
    /// While probing in place, everything but the probe's answer is
    /// dropped.
    probing: AtomicBool,
    era: Mutex<Option<Era>>,
    /// Whether the server's tool-list changes are heard.
    hear_tools: AtomicBool,
    ack: Mutex<Option<oneshot::Sender<()>>>,
    options: ClientOptions,
}

/// One connection to one server. (SDK: `Client`.) A client connects once;
/// a supervisor makes a new one, over a new transport, for every attempt.
#[derive(Clone)]
pub struct Client {
    peer: Arc<Peer>,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    /// The transport's inbound channel, until the read loop takes it.
    attached: bool,
    /// `close` was called.
    closing: bool,
    next_id: u64,
    next_probe: u64,
    capabilities: Value,
    server_info: Value,
    instructions: Option<String>,
}

fn key(id: &Value) -> String {
    id.to_string()
}

impl Client {
    pub fn new(transport: Box<dyn Transport>, options: ClientOptions) -> Self {
        Self {
            peer: Arc::new(Peer::new(Arc::from(transport), options)),
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The server's instructions, as it sent them. (SDK:
    /// `getInstructions`.)
    pub fn instructions(&self) -> Option<String> {
        self.lock().instructions.clone()
    }

    /// The server's `capabilities`. (SDK: `getServerCapabilities`.)
    pub fn server_capabilities(&self) -> Value {
        self.lock().capabilities.clone()
    }

    /// The server's `serverInfo`. (SDK: `getServerVersion`.)
    pub fn server_info(&self) -> Value {
        self.lock().server_info.clone()
    }

    /// The era the connection negotiated, once it has.
    pub fn era(&self) -> Option<Era> {
        self.peer.era().clone()
    }

    /// Whether the transport has said the connection closed.
    pub fn is_closed(&self) -> bool {
        *self.peer.closed.borrow()
    }

    /// Resolves once the transport has said the connection closed.
    pub async fn closed(&self) {
        let mut closed = self.peer.closed.subscribe();
        let _ = closed.wait_for(|closed| *closed).await;
    }

    /// Negotiates the era and runs its handshake. (SDK: `connect`, with
    /// `versionNegotiation: { mode: 'auto' }`.)
    ///
    /// # Errors
    ///
    /// The transport didn't start, the probe failed in a way that isn't a
    /// legacy fallback, or the handshake failed; the transport is closed
    /// then. Each in the SDK's words.
    pub async fn connect(&self) -> Result<(), McpError> {
        let transport = Arc::clone(&self.peer.transport);
        let verdict = match transport.sibling() {
            Some(sibling) if transport.kind() == TransportKind::Stdio => {
                let verdict = self.probe_on_sibling(sibling).await;
                if self.lock().closing {
                    return Err(McpError::new(
                        ErrorKind::EraNegotiationFailed,
                        "Version negotiation failed: the transport was closed during the server/discover probe",
                    ));
                }
                let verdict = verdict?;
                self.attach().await?;
                verdict
            }
            _ => {
                self.peer.probing.store(true, Ordering::SeqCst);
                let verdict = match self.attach().await {
                    Ok(()) => self.probe(&self.peer, transport.kind()).await,
                    Err(error) => Err(error),
                };
                self.peer.probing.store(false, Ordering::SeqCst);
                match verdict {
                    Ok(verdict) => verdict,
                    Err(error) => {
                        let _ = transport.close().await;
                        return Err(error);
                    }
                }
            }
        };
        let handshake = match verdict {
            Verdict::Modern(discover) => {
                self.adopt(&discover);
                Ok(())
            }
            Verdict::Legacy => self.legacy_handshake().await,
        };
        if let Err(error) = handshake {
            // As the SDK closes on a failed handshake, without waiting.
            let transport = Arc::clone(&transport);
            tokio::spawn(async move {
                let _ = transport.close().await;
            });
            return Err(error);
        }
        Ok(())
    }

    /// Starts the transport and its read loop.
    async fn attach(&self) -> Result<(), McpError> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.peer.transport.start(sender).await?;
        self.lock().attached = true;
        tokio::spawn(read_loop(Arc::clone(&self.peer), receiver));
        Ok(())
    }

    /// The probe on a disposable twin, reaped whatever it found.
    async fn probe_on_sibling(&self, sibling: Box<dyn Transport>) -> Result<Verdict, McpError> {
        let twin = Arc::new(Peer::new(Arc::from(sibling), ClientOptions::default()));
        twin.probing.store(true, Ordering::SeqCst);
        let (sender, receiver) = mpsc::unbounded_channel();
        twin.transport.start(sender).await?;
        tokio::spawn(read_loop(Arc::clone(&twin), receiver));
        let verdict = self.probe(&twin, TransportKind::Stdio).await;
        let _ = twin.transport.close().await;
        verdict
    }

    /// Sends `server/discover` through `peer` and reads its outcome.
    /// (SDK: `buildProbeRequest` and `classifyProbeOutcome`.)
    async fn probe(&self, peer: &Peer, kind: TransportKind) -> Result<Verdict, McpError> {
        let id = {
            let mut state = self.lock();
            state.next_probe += 1;
            Value::String(format!("server-discover-probe-{}", state.next_probe))
        };
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "server/discover",
            "params": { "_meta": envelope() },
        });
        let timeout = self
            .peer
            .options
            .probe_timeout
            .unwrap_or(Duration::from_millis(crate::DEFAULT_REQUEST_TIMEOUT_MS));
        let outcome = peer.exchange(&id, message, timeout, None).await;
        classify(outcome, kind, timeout)
    }

    /// Takes the modern verdict's `server/discover` result. (SDK: the
    /// modern arm of `_connectNegotiated`.)
    fn adopt(&self, discover: &Value) {
        {
            let mut state = self.lock();
            state.capabilities = discover.get("capabilities").cloned().unwrap_or(json!({}));
            state.server_info = discover
                .pointer("/_meta")
                .and_then(|meta| meta.get(meta::SERVER_INFO))
                .cloned()
                .unwrap_or(Value::Null);
            state.instructions = discover
                .get("instructions")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        self.peer
            .transport
            .set_protocol_version(MODERN_PROTOCOL_VERSION);
        *self.peer.era() = Some(Era::Modern);
        if self.wants_tool_changes() {
            self.peer.hear_tools.store(true, Ordering::SeqCst);
            tokio::spawn(listen(Arc::clone(&self.peer)));
        }
    }

    /// The 2025-era handshake. (SDK: `_legacyHandshake`.)
    async fn legacy_handshake(&self) -> Result<(), McpError> {
        let result = self
            .request(
                "initialize",
                Some(json!({
                    "protocolVersion": LATEST_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": client_info(),
                })),
                RequestOptions::default(),
            )
            .await?;
        if !result.is_object() {
            return Err(McpError::other(format!(
                "Server sent invalid initialize result: {result}"
            )));
        }
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version.as_str()) {
            return Err(McpError::other(format!(
                "Server's protocol version is not supported: {version}"
            )));
        }
        {
            let mut state = self.lock();
            state.capabilities = result.get("capabilities").cloned().unwrap_or(json!({}));
            state.server_info = result.get("serverInfo").cloned().unwrap_or(Value::Null);
            state.instructions = result
                .get("instructions")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        self.peer.transport.set_protocol_version(&version);
        self.notify("notifications/initialized", None).await?;
        *self.peer.era() = Some(Era::Legacy(version));
        if self.wants_tool_changes() {
            self.peer.hear_tools.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Whether a tools handler is set and the server declares
    /// `tools.listChanged`. (SDK: `config.tools &&
    /// this._serverCapabilities?.tools?.listChanged`.)
    fn wants_tool_changes(&self) -> bool {
        self.peer.options.on_tools_changed.is_some()
            && self
                .lock()
                .capabilities
                .pointer("/tools/listChanged")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    }

    /// Sends a request and waits for its result. (SDK: `Protocol.request`.)
    ///
    /// # Errors
    ///
    /// `Not connected`; a method the modern era has no place for; the
    /// server's JSON-RPC error, its message as the server wrote it;
    /// `Request timed out`, or the cancel's reason, both as
    /// [`ErrorKind::RequestTimeout`] as in the SDK; `Connection closed`;
    /// or the transport's failure to send.
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        options: RequestOptions,
    ) -> Result<Value, McpError> {
        let (id, attached) = {
            let mut state = self.lock();
            let id = state.next_id;
            state.next_id += 1;
            (id, state.attached)
        };
        if !attached || self.is_closed() {
            return Err(McpError::not_connected());
        }
        let modern = matches!(*self.peer.era(), Some(Era::Modern));
        if modern && !MODERN_METHODS.contains(&method) {
            return Err(McpError::new(
                ErrorKind::MethodNotSupportedByProtocolVersion,
                format!(
                    "Method '{method}' is not supported by the negotiated protocol version (wire era {MODERN_PROTOCOL_VERSION})"
                ),
            ));
        }
        if let Some(cancel) = &options.cancel
            && cancel.is_aborted()
        {
            return Err(aborted());
        }
        let id = Value::from(id);
        let mut message = Map::new();
        message.insert("jsonrpc".into(), "2.0".into());
        message.insert("id".into(), id.clone());
        message.insert("method".into(), method.into());
        if let Some(params) = with_envelope(params, modern) {
            message.insert("params".into(), params);
        }
        let timeout = options
            .timeout
            .unwrap_or(Duration::from_millis(crate::DEFAULT_REQUEST_TIMEOUT_MS));
        let mut dropped = CancelOnDrop {
            client: (method != "initialize").then(|| self.clone()),
            id: id.clone(),
        };
        let outcome = self
            .peer
            .exchange(
                &id,
                Value::Object(message),
                timeout,
                options.cancel.as_ref(),
            )
            .await;
        dropped.client = None;
        match outcome {
            Outcome::Response(response) => match response.get("error") {
                Some(error) => Err(McpError::protocol(error)),
                None => Ok(response.get("result").cloned().unwrap_or(Value::Null)),
            },
            Outcome::Closed => Err(McpError::closed()),
            Outcome::Send(error) => Err(error),
            Outcome::Timeout => {
                let error = McpError::new(ErrorKind::RequestTimeout, "Request timed out");
                if method != "initialize" {
                    self.cancelled(&id, &error.display_with_name()).await;
                }
                Err(error)
            }
            Outcome::Cancelled => {
                let error = aborted();
                if method != "initialize" {
                    self.cancelled(&id, &error.message).await;
                }
                Err(error)
            }
        }
    }

    /// Tells the server a request was given up, where the SDK does: on a
    /// 2025-era connection, or on stdio. A modern HTTP request is ended by
    /// its own stream instead.
    async fn cancelled(&self, id: &Value, reason: &str) {
        let modern = matches!(*self.peer.era(), Some(Era::Modern));
        if modern && self.peer.transport.kind() == TransportKind::Http {
            return;
        }
        let _ = self
            .notify(
                "notifications/cancelled",
                Some(json!({ "requestId": id, "reason": reason })),
            )
            .await;
    }

    /// Sends a notification.
    ///
    /// # Errors
    ///
    /// `Not connected`, or the transport's failure to send.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        if !self.lock().attached || self.is_closed() {
            return Err(McpError::not_connected());
        }
        let modern = matches!(*self.peer.era(), Some(Era::Modern));
        let mut message = Map::new();
        message.insert("jsonrpc".into(), "2.0".into());
        message.insert("method".into(), method.into());
        if let Some(params) = with_envelope(params, modern) {
            message.insert("params".into(), params);
        }
        self.peer.transport.send(Value::Object(message)).await
    }

    /// One page of the server's tools, from `cursor`. (SDK: `listTools`
    /// with an explicit cursor.)
    ///
    /// # Errors
    ///
    /// As [`request`](Self::request).
    pub async fn list_tools(
        &self,
        cursor: Option<&str>,
        options: RequestOptions,
    ) -> Result<ToolsPage, McpError> {
        let params = cursor.map(|cursor| json!({ "cursor": cursor }));
        let result = self.request("tools/list", params, options).await?;
        Ok(ToolsPage {
            tools: result
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            next_cursor: result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned),
            result,
        })
    }

    /// Every tool, walking the pages, at most 64 of them; none for a server
    /// that doesn't declare `tools`. (SDK: `listTools` with no cursor.)
    ///
    /// # Errors
    ///
    /// As [`request`](Self::request).
    pub async fn list_all_tools(&self, options: RequestOptions) -> Result<Vec<Value>, McpError> {
        if self.server_capabilities().get("tools").is_none() {
            return Ok(Vec::new());
        }
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let page = self.list_tools(cursor.as_deref(), options.clone()).await?;
            tools.extend(page.tools);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(tools)
    }

    /// Calls a tool. (SDK: `callTool`, without its output-schema check,
    /// which belongs with the tool definitions.)
    ///
    /// # Errors
    ///
    /// As [`request`](Self::request).
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        options: RequestOptions,
    ) -> Result<Value, McpError> {
        self.request(
            "tools/call",
            Some(json!({ "name": name, "arguments": arguments })),
            options,
        )
        .await
    }

    /// Closes the transport. (SDK: `Client.close`.)
    ///
    /// # Errors
    ///
    /// The transport's close failed.
    pub async fn close(&self) -> Result<(), McpError> {
        let attached = {
            let mut state = self.lock();
            state.closing = true;
            state.attached
        };
        if attached
            && self.peer.hear_tools.load(Ordering::SeqCst)
            && matches!(*self.peer.era(), Some(Era::Modern))
            && self.peer.transport.kind() == TransportKind::Stdio
        {
            let _ = self
                .notify(
                    "notifications/cancelled",
                    Some(json!({ "requestId": LISTEN_ID })),
                )
                .await;
        }
        self.peer.transport.close().await
    }

    /// Closes the client and says whether the closure is confirmed: for a
    /// started transport, that it reported closing within
    /// [`CLOSE_CONFIRM_TIMEOUT`]; for one never started, that its close
    /// succeeded. (dsh: `closeGeneration` and `waitForClose`.)
    pub async fn shutdown(&self) -> bool {
        let attached = self.lock().attached;
        let closed = self.close().await;
        if !attached {
            return closed.is_ok() || self.is_closed();
        }
        self.is_closed()
            || tokio::time::timeout(CLOSE_CONFIRM_TIMEOUT, self.closed())
                .await
                .is_ok()
    }
}

/// The envelope's `_meta` under `params`, for a modern message, the
/// caller's own `_meta` keys winning. (SDK: `_outboundMetaEnvelope`.)
fn with_envelope(params: Option<Value>, modern: bool) -> Option<Value> {
    if !modern {
        return params;
    }
    let mut params = match params {
        Some(Value::Object(params)) => params,
        _ => Map::new(),
    };
    let mut meta = envelope().as_object().cloned().unwrap_or_default();
    if let Some(Value::Object(own)) = params.remove("_meta") {
        meta.extend(own);
    }
    params.insert("_meta".into(), Value::Object(meta));
    Some(Value::Object(params))
}

/// A cancelled request's error: the SDK rejects with
/// `SdkError(REQUEST_TIMEOUT, String(signal.reason))`.
fn aborted() -> McpError {
    McpError::new(
        ErrorKind::RequestTimeout,
        "AbortError: This operation was aborted",
    )
}

/// Takes a request's entry out of the pending map when its wait ends,
/// however it ends.
struct Forget<'a> {
    peer: &'a Peer,
    key: &'a str,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.peer.pending().remove(self.key);
    }
}

/// Tells the server a request was given up when its caller drops it
/// before it settles: a Karaka tool call is cancelled by dropping its
/// future, where dsh's aborts its signal.
struct CancelOnDrop {
    client: Option<Client>,
    id: Value,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(client) = self.client.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let id = self.id.clone();
        runtime.spawn(async move {
            client.cancelled(&id, &aborted().message).await;
        });
    }
}

/// What one request came to.
enum Outcome {
    Response(Value),
    Closed,
    Timeout,
    Cancelled,
    Send(McpError),
}

impl Peer {
    fn new(transport: Arc<dyn Transport>, options: ClientOptions) -> Self {
        Self {
            transport,
            pending: Mutex::new(HashMap::new()),
            closed: watch::channel(false).0,
            probing: AtomicBool::new(false),
            era: Mutex::new(None),
            hear_tools: AtomicBool::new(false),
            ack: Mutex::new(None),
            options,
        }
    }

    fn era(&self) -> MutexGuard<'_, Option<Era>> {
        self.era.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pending(&self) -> MutexGuard<'_, HashMap<String, oneshot::Sender<Answer>>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn error(&self, message: String) {
        if let Some(on_error) = &self.options.on_error {
            on_error(message);
        }
    }

    /// Sends `message`, whose id is `id`, and waits for its response.
    async fn exchange(
        &self,
        id: &Value,
        message: Value,
        timeout: Duration,
        cancel: Option<&AbortSignal>,
    ) -> Outcome {
        let (sender, receiver) = oneshot::channel();
        let key = key(id);
        if *self.closed.borrow() {
            return Outcome::Closed;
        }
        self.pending().insert(key.clone(), sender);
        // Dropped, the wait forgets its entry, so a late answer is unknown.
        let _forget = Forget {
            peer: self,
            key: &key,
        };
        if let Err(error) = self.transport.send(message).await {
            return Outcome::Send(error);
        }
        let never = AbortSignal::never();
        let cancel = cancel.unwrap_or(&never);
        let outcome = tokio::select! {
            answer = receiver => match answer {
                Ok(Answer::Response(response)) => Outcome::Response(response),
                Ok(Answer::Closed) | Err(_) => Outcome::Closed,
            },
            () = tokio::time::sleep(timeout) => Outcome::Timeout,
            () = cancel.wait() => Outcome::Cancelled,
        };
        outcome
    }

    /// Answers one request the server sent. (SDK: `ping`'s default
    /// handler, and `-32601 Method not found` for the rest.)
    async fn answer(&self, request: &Value) {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let era = self.era().clone();
        if era == Some(Era::Modern) && self.transport.kind() == TransportKind::Stdio {
            self.error(format!(
                "Dropped inbound request '{method}': not servable on this connection's protocol era"
            ));
            return;
        }
        let reply = if method == "ping" && era != Some(Era::Modern) {
            json!({ "jsonrpc": "2.0", "id": id, "result": {} })
        } else {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": code::METHOD_NOT_FOUND, "message": "Method not found" },
            })
        };
        if let Err(error) = self.transport.send(reply).await {
            self.error(error.message);
        }
    }

    fn notified(&self, notification: &Value) {
        let method = notification
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match method {
            "notifications/tools/list_changed" => {
                if self.hear_tools.load(Ordering::SeqCst)
                    && let Some(changed) = &self.options.on_tools_changed
                {
                    changed();
                }
            }
            "notifications/subscriptions/acknowledged" => {
                let id = notification
                    .pointer("/params/_meta")
                    .and_then(|meta| meta.get(meta::SUBSCRIPTION_ID))
                    .and_then(Value::as_str);
                if id == Some(LISTEN_ID)
                    && let Some(ack) = self
                        .ack
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .take()
                {
                    let _ = ack.send(());
                }
            }
            _ => {}
        }
    }
}

/// Reads what the transport delivers until it closes. (SDK: `Protocol`'s
/// `_onmessage`, `_onerror` and `_onclose`.)
async fn read_loop(peer: Arc<Peer>, mut inbound: UnboundedReceiver<Inbound>) {
    while let Some(event) = inbound.recv().await {
        match event {
            Inbound::Message(message) => {
                let is_response = message.get("method").is_none()
                    && (message.get("result").is_some() || message.get("error").is_some());
                if is_response {
                    let id = message.get("id").cloned().unwrap_or(Value::Null);
                    let waiting = peer.pending().remove(&key(&id));
                    match waiting {
                        Some(waiting) => {
                            let _ = waiting.send(Answer::Response(message));
                        }
                        None if key(&id) == key(&Value::from(LISTEN_ID)) => {}
                        None if peer.probing.load(Ordering::SeqCst) => {}
                        None => peer.error(format!(
                            "Received a response for an unknown message ID: {message}"
                        )),
                    }
                    continue;
                }
                if peer.probing.load(Ordering::SeqCst) {
                    continue;
                }
                if message.get("id").is_some() {
                    peer.answer(&message).await;
                } else {
                    peer.notified(&message);
                }
            }
            Inbound::Error(message) => peer.error(message),
            Inbound::Closed => break,
        }
    }
    peer.closed.send_replace(true);
    if let Some(on_close) = &peer.options.on_close {
        on_close();
    }
    let waiting: Vec<_> = peer.pending().drain().map(|(_, waiting)| waiting).collect();
    for waiting in waiting {
        let _ = waiting.send(Answer::Closed);
    }
}

/// Opens the modern era's change subscription, and reports it if the
/// server doesn't acknowledge it. (SDK: `subscriptions/listen`, opened
/// after connect.)
async fn listen(peer: Arc<Peer>) {
    let (ack, acked) = oneshot::channel();
    *peer.ack.lock().unwrap_or_else(PoisonError::into_inner) = Some(ack);
    let message = json!({
        "jsonrpc": "2.0",
        "id": LISTEN_ID,
        "method": "subscriptions/listen",
        "params": { "_meta": envelope(), "notifications": { "toolsListChanged": true } },
    });
    if let Err(error) = peer.transport.send(message).await {
        peer.error(error.message);
        return;
    }
    let timeout = Duration::from_millis(crate::DEFAULT_REQUEST_TIMEOUT_MS);
    if tokio::time::timeout(timeout, acked).await.is_err() {
        peer.error("subscriptions/listen ack timed out".into());
    }
}

/// What the probe found.
enum Verdict {
    /// The `server/discover` result.
    Modern(Value),
    Legacy,
}

/// The probe's outcome as a verdict, or the connection's failure. (SDK:
/// `classifyProbeOutcome`.)
fn classify(outcome: Outcome, kind: TransportKind, timeout: Duration) -> Result<Verdict, McpError> {
    let failed = |message: String| Err(McpError::new(ErrorKind::EraNegotiationFailed, message));
    match outcome {
        Outcome::Response(response) => match response.get("error") {
            // Any JSON-RPC error reads as a 2025-era server. The SDK's one
            // corrective retry, for an `UnsupportedProtocolVersion` naming a
            // mutual modern revision, isn't ported: with one modern revision
            // there is nothing else to retry with.
            Some(_) => Ok(Verdict::Legacy),
            None => {
                let result = response.get("result").cloned().unwrap_or(Value::Null);
                let offers = result
                    .get("supportedVersions")
                    .and_then(Value::as_array)
                    .is_some_and(|versions| {
                        versions.iter().all(Value::is_string)
                            && versions.iter().any(|v| v == MODERN_PROTOCOL_VERSION)
                    });
                let shaped = result.get("capabilities").is_some_and(Value::is_object);
                Ok(if offers && shaped {
                    Verdict::Modern(result)
                } else {
                    Verdict::Legacy
                })
            }
        },
        Outcome::Send(error) => match error.kind {
            ErrorKind::Http(401) => failed(
                "Version negotiation failed: the server requires authorization (HTTP 401)".into(),
            ),
            ErrorKind::Http(403) => {
                failed("Version negotiation failed: the server denied access (HTTP 403)".into())
            }
            ErrorKind::Http(status) if status >= 500 => failed(format!(
                "Version negotiation failed: the server answered the probe with HTTP {status}"
            )),
            // Another 4xx: the SDK reads its body as a JSON-RPC error, and
            // every such error, like a body that isn't one, is legacy.
            ErrorKind::Http(_) => Ok(Verdict::Legacy),
            _ => failed(format!(
                "Version negotiation probe failed: {}",
                error.message
            )),
        },
        Outcome::Closed => match kind {
            TransportKind::Stdio => Ok(Verdict::Legacy),
            TransportKind::Http => Err(McpError::transport(
                "Connection closed during the version negotiation probe",
            )),
        },
        Outcome::Timeout => match kind {
            TransportKind::Stdio => Ok(Verdict::Legacy),
            TransportKind::Http => Err(McpError::new(
                ErrorKind::RequestTimeout,
                format!(
                    "Version negotiation probe timed out after {}ms",
                    timeout.as_millis()
                ),
            )),
        },
        Outcome::Cancelled => Ok(Verdict::Legacy),
    }
}
