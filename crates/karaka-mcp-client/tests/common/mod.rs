//! A scripted in-process server, as a transport, and a recording observer.
//! (dsh: the SDK's `InMemoryTransport` with an `McpServer`, and the mocked
//! `Client` of `reconnect.spec.ts`.)

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use karaka_mcp_client::{
    Client, ConnectionObserver, ErrorKind, Inbound, Level, McpError, RequestOptions, SyncFuture,
    Transport, TransportFactory, TransportFuture, TransportKind,
};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

/// What the scripted server does with one message.
pub enum Reply {
    /// Answer with this result.
    Result(Value),
    /// Answer with this JSON-RPC error.
    Error(i64, &'static str),
    /// Say nothing.
    Silent,
    /// Fail the send, as an HTTP status or a network error would.
    Fail(McpError),
}

pub type Script = Arc<dyn Fn(&Value) -> Reply + Send + Sync>;

/// The scripted server's knobs, shared by every transport a factory makes.
#[derive(Clone)]
pub struct Knobs {
    pub kind: TransportKind,
    /// `start` fails with this.
    pub start_error: Option<McpError>,
    /// How many `close` calls fail, from the first.
    pub failing_closes: Arc<AtomicUsize>,
    /// Whether `close` reports the closure.
    pub close_reports: bool,
    /// Whether the transport has a probe twin.
    pub sibling: bool,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            kind: TransportKind::Stdio,
            start_error: None,
            failing_closes: Arc::new(AtomicUsize::new(0)),
            close_reports: true,
            sibling: false,
        }
    }
}

/// One scripted connection, which a test can push on.
pub struct Scripted {
    pub script: Mutex<Script>,
    pub knobs: Knobs,
    pub inbound: Mutex<Option<UnboundedSender<Inbound>>>,
    pub sent: Mutex<Vec<Value>>,
    pub closes: AtomicUsize,
    /// The twin's own record, once a probe made one.
    pub twin: Mutex<Option<Arc<Scripted>>>,
}

impl Scripted {
    pub fn new(script: Script, knobs: Knobs) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            knobs,
            inbound: Mutex::new(None),
            sent: Mutex::new(Vec::new()),
            closes: AtomicUsize::new(0),
            twin: Mutex::new(None),
        })
    }

    pub fn sent(&self) -> Vec<Value> {
        self.sent.lock().unwrap().clone()
    }

    pub fn methods(&self) -> Vec<String> {
        self.sent()
            .iter()
            .filter_map(|message| message["method"].as_str().map(str::to_owned))
            .collect()
    }

    /// The server pushes `message`.
    pub fn push(&self, message: Value) {
        if let Some(inbound) = &*self.inbound.lock().unwrap() {
            let _ = inbound.send(Inbound::Message(message));
        }
    }

    /// The server's side goes away.
    pub fn drop_connection(&self) {
        if let Some(inbound) = self.inbound.lock().unwrap().take() {
            let _ = inbound.send(Inbound::Closed);
        }
    }

    pub fn set_script(&self, script: Script) {
        *self.script.lock().unwrap() = script;
    }
}

/// A transport over one [`Scripted`].
pub struct ScriptedTransport(pub Arc<Scripted>);

impl Transport for ScriptedTransport {
    fn kind(&self) -> TransportKind {
        self.0.knobs.kind
    }

    fn start(
        &self,
        inbound: UnboundedSender<Inbound>,
    ) -> TransportFuture<'_, Result<(), McpError>> {
        Box::pin(async move {
            if let Some(error) = &self.0.knobs.start_error {
                return Err(error.clone());
            }
            *self.0.inbound.lock().unwrap() = Some(inbound);
            Ok(())
        })
    }

    fn send(&self, message: Value) -> TransportFuture<'_, Result<(), McpError>> {
        Box::pin(async move {
            self.0.sent.lock().unwrap().push(message.clone());
            let id = message.get("id").cloned();
            let script = self.0.script.lock().unwrap().clone();
            let reply = script(&message);
            let Some(id) = id.filter(|_| message.get("method").is_some()) else {
                return Ok(());
            };
            match reply {
                Reply::Result(result) => {
                    self.0
                        .push(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                }
                Reply::Error(code, text) => self.0.push(json!({
                    "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": text },
                })),
                Reply::Silent => {}
                Reply::Fail(error) => return Err(error),
            }
            Ok(())
        })
    }

    fn close(&self) -> TransportFuture<'_, Result<(), McpError>> {
        Box::pin(async move {
            self.0.closes.fetch_add(1, Ordering::SeqCst);
            let failing = self.0.knobs.failing_closes.load(Ordering::SeqCst);
            if failing > 0 {
                self.0.knobs.failing_closes.fetch_sub(1, Ordering::SeqCst);
                return Err(McpError::transport("shutdown failed"));
            }
            if self.0.knobs.close_reports {
                self.0.drop_connection();
            }
            Ok(())
        })
    }

    fn sibling(&self) -> Option<Box<dyn Transport>> {
        if !self.0.knobs.sibling {
            return None;
        }
        let script = self.0.script.lock().unwrap().clone();
        let twin = Scripted::new(
            script,
            Knobs {
                sibling: false,
                ..self.0.knobs.clone()
            },
        );
        *self.0.twin.lock().unwrap() = Some(Arc::clone(&twin));
        Some(Box::new(ScriptedTransport(twin)))
    }
}

/// A 2025-era server: `server/discover` is unknown to it, and it has
/// `tools`. (dsh: the fixtures' legacy servers.)
pub fn legacy_server(tools: &[&str]) -> Script {
    let tools: Vec<Value> = tools
        .iter()
        .map(|name| json!({ "name": name, "inputSchema": { "type": "object" } }))
        .collect();
    Arc::new(
        move |message| match message["method"].as_str().unwrap_or_default() {
            "server/discover" => Reply::Error(-32601, "Method not found"),
            "initialize" => Reply::Result(json!({
                "protocolVersion": message["params"]["protocolVersion"],
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": "fixture", "version": "1" },
            })),
            "tools/list" => Reply::Result(json!({ "tools": tools.clone() })),
            "tools/call" => Reply::Result(json!({ "content": [{ "type": "text", "text": "ok" }] })),
            _ => Reply::Silent,
        },
    )
}

/// A server whose `initialize` fails.
pub fn failing_server(message: &'static str) -> Script {
    Arc::new(
        move |request| match request["method"].as_str().unwrap_or_default() {
            "server/discover" => Reply::Error(-32601, "Method not found"),
            "initialize" => Reply::Error(-32603, message),
            _ => Reply::Silent,
        },
    )
}

/// A factory of scripted transports, keeping each one.
#[derive(Clone)]
pub struct Factory {
    pub script: Arc<Mutex<Script>>,
    pub knobs: Knobs,
    pub made: Arc<Mutex<Vec<Arc<Scripted>>>>,
}

impl Factory {
    pub fn new(script: Script, knobs: Knobs) -> Self {
        Self {
            script: Arc::new(Mutex::new(script)),
            knobs,
            made: Arc::default(),
        }
    }

    /// What the next transports' servers do.
    pub fn set_script(&self, script: Script) {
        *self.script.lock().unwrap() = script;
    }

    pub fn made(&self) -> Vec<Arc<Scripted>> {
        self.made.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.made.lock().unwrap().len()
    }

    pub fn factory(&self) -> TransportFactory {
        let this = self.clone();
        Arc::new(move || -> Box<dyn Transport> {
            let script = this.script.lock().unwrap().clone();
            let scripted = Scripted::new(script, this.knobs.clone());
            this.made.lock().unwrap().push(Arc::clone(&scripted));
            Box::new(ScriptedTransport(scripted))
        })
    }
}

/// An observer that publishes each generation's tool names and keeps the
/// supervisor's lines.
#[derive(Default)]
pub struct Recorder {
    pub published: Mutex<Vec<String>>,
    pub syncs: AtomicUsize,
    pub clears: AtomicUsize,
    pub lines: Mutex<Vec<(Level, String)>>,
    /// While set, a sync waits for it.
    pub gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
}

impl Recorder {
    pub fn lines(&self, level: Level) -> Vec<String> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .filter(|(at, _)| *at == level)
            .map(|(_, line)| line.clone())
            .collect()
    }

    pub fn published(&self) -> Vec<String> {
        self.published.lock().unwrap().clone()
    }

    pub fn has(&self, level: Level, part: &str) -> bool {
        self.lines(level).iter().any(|line| line.contains(part))
    }
}

impl ConnectionObserver for Recorder {
    fn sync<'a>(&'a self, client: &'a Client, _strict: bool) -> SyncFuture<'a> {
        Box::pin(async move {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            let tools = client.list_all_tools(RequestOptions::default()).await?;
            let gate = self.gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            *self.published.lock().unwrap() = tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                .collect();
            Ok(())
        })
    }

    fn clear(&self) {
        self.clears.fetch_add(1, Ordering::SeqCst);
        self.published.lock().unwrap().clear();
    }

    fn log(&self, level: Level, message: &str) {
        self.lines.lock().unwrap().push((level, message.to_owned()));
    }
}

/// Waits up to five seconds for `condition`.
pub async fn wait_for(mut condition: impl FnMut() -> bool) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(condition(), "the condition never held");
}

pub fn http_error(status: u16) -> McpError {
    McpError {
        kind: ErrorKind::Http(status),
        message: format!("Error POSTing to endpoint: status {status}"),
        data: Some(json!({ "status": status, "text": "" })),
    }
}
