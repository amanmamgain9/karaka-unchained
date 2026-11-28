//! The stdio transport: a server run as a child process, speaking
//! newline-delimited JSON-RPC on its stdin and stdout. (SDK:
//! `StdioClientTransport`, `client/dist/stdio.mjs`; dsh: `createTransport`'s
//! stdio arm, with `scrubbedParentEnv`.)

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::protocol::{McpError, STDIO_MAX_BUFFER_BYTES};
use crate::transport::{Inbound, Transport, TransportFuture, TransportKind};

/// How long `close` waits for the server to exit after its stdin ends, and
/// again after SIGTERM, before SIGKILL. (SDK: two 2000 ms waits.)
const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// How long a probe twin gets after SIGTERM. (SDK: the sibling's
/// `_dispose`, 1000 ms on `exit`.)
const SIBLING_WAIT: Duration = Duration::from_secs(1);

/// What becomes of the server's stderr. (SDK: `stderr`, `inherit` by
/// default.)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stderr {
    /// The server writes to this process's stderr, as dsh's servers do.
    #[default]
    Inherit,
    /// Discarded, as for the probe twin.
    Ignore,
}

/// How to run a stdio server. (dsh: `StdioConfig`'s `command`, `args`,
/// `env` and `cwd`.)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StdioParams {
    /// The executable, run directly, with no shell.
    pub command: String,
    pub args: Vec<String>,
    /// Set on top of the scrubbed environment of this process.
    pub env: BTreeMap<String, String>,
    /// The child's directory; this process's when empty.
    pub cwd: String,
    pub stderr: Stderr,
}

/// The environment a server starts with: this process's, without the names
/// that look like credentials or are dsh's own, then `extra`. (dsh:
/// `scrubbedParentEnv`, dropping names matching `KEY|PASSWORD|SECRET|TOKEN`
/// in any case and every `DSH_*` name, then the spec's `env`.)
///
/// dsh also restores proxy variables its own proxy policy normalized.
/// Karaka has no proxy policy, so the inherited ones pass through as they
/// are.
pub fn child_env(extra: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let sensitive = |name: &str| {
        let upper = name.to_uppercase();
        ["KEY", "PASSWORD", "SECRET", "TOKEN"]
            .iter()
            .any(|word| upper.contains(word))
            || upper.starts_with("DSH_")
    };
    let mut env: BTreeMap<String, String> = std::env::vars()
        .filter(|(name, _)| !sensitive(name))
        .collect();
    env.extend(
        extra
            .iter()
            .map(|(name, value)| (name.clone(), value.clone())),
    );
    env
}

/// A running server.
struct Running {
    stdin: Option<Arc<tokio::sync::Mutex<ChildStdin>>>,
    pid: Option<u32>,
    /// Tells the process's task to SIGKILL it.
    kill: mpsc::UnboundedSender<()>,
    /// `true` once the process has exited.
    exited: watch::Receiver<bool>,
}

/// The stdio transport. (SDK: `StdioClientTransport`.)
pub struct StdioTransport {
    params: StdioParams,
    /// A probe twin: closed with SIGTERM at once.
    sibling: bool,
    state: Mutex<State>,
}

#[derive(Default)]
enum State {
    #[default]
    New,
    Running(Running),
    Closed,
}

impl StdioTransport {
    pub fn new(params: StdioParams) -> Self {
        Self {
            params,
            sibling: false,
            state: Mutex::new(State::New),
        }
    }

    /// The process's id, while it runs.
    pub fn pid(&self) -> Option<u32> {
        match &*self.lock() {
            State::Running(running) => running.pid,
            _ => None,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn spawn(&self, inbound: UnboundedSender<Inbound>) -> Result<(), McpError> {
        let mut state = self.lock();
        if !matches!(*state, State::New) {
            return Err(McpError::transport(
                "StdioClientTransport already started! If using Client class, note that connect() calls start() automatically.",
            ));
        }
        let params = &self.params;
        let mut command = Command::new(&params.command);
        command
            .args(&params.args)
            .env_clear()
            .envs(child_env(&params.env))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(match params.stderr {
                Stderr::Inherit => Stdio::inherit(),
                Stderr::Ignore => Stdio::null(),
            })
            .kill_on_drop(true);
        if !params.cwd.is_empty() {
            command.current_dir(&params.cwd);
        }
        let mut child = command
            .spawn()
            .map_err(|error| McpError::transport(format!("spawn {} {error}", params.command)))?;
        let stdin = child
            .stdin
            .take()
            .map(|stdin| Arc::new(tokio::sync::Mutex::new(stdin)));
        let stdout = child.stdout.take();
        let pid = child.id();
        let (kill, killed) = mpsc::unbounded_channel();
        let reader = stdout.map(|stdout| tokio::spawn(read(stdout, inbound.clone(), kill.clone())));
        let (exit, exited) = watch::channel(false);
        tokio::spawn(supervise(child, reader, killed, exit, inbound));
        *state = State::Running(Running {
            stdin,
            pid,
            kill,
            exited,
        });
        Ok(())
    }

    async fn shut(&self) -> Result<(), McpError> {
        let running = match std::mem::replace(&mut *self.lock(), State::Closed) {
            State::Running(running) => running,
            _ => return Ok(()),
        };
        let Running {
            stdin,
            pid,
            kill,
            mut exited,
        } = running;
        let terminate = || {
            if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
                // SAFETY: `kill` with a child's pid and SIGTERM has no
                // memory effects; a pid already reaped fails with ESRCH.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        };
        let wait = |exited: &mut watch::Receiver<bool>, limit| {
            let mut exited = exited.clone();
            async move {
                tokio::time::timeout(limit, exited.wait_for(|exited| *exited))
                    .await
                    .is_ok()
            }
        };
        if self.sibling {
            // The probe twin is reaped at once. (SDK: `_dispose`.)
            drop(stdin);
            terminate();
            if !wait(&mut exited, SIBLING_WAIT).await {
                let _ = kill.send(());
            }
            return Ok(());
        }
        // The server's stdin ends first, then it gets SIGTERM, then SIGKILL.
        drop(stdin);
        if wait(&mut exited, CLOSE_WAIT).await {
            return Ok(());
        }
        terminate();
        if wait(&mut exited, CLOSE_WAIT).await {
            return Ok(());
        }
        let _ = kill.send(());
        Ok(())
    }

    async fn write(&self, message: Value) -> Result<(), McpError> {
        let stdin = match &*self.lock() {
            State::Running(running) => running.stdin.clone(),
            _ => None,
        };
        let Some(stdin) = stdin else {
            return Err(McpError::not_connected());
        };
        let mut line = message.to_string();
        line.push('\n');
        let mut stdin = stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| McpError::transport(error.to_string()))?;
        stdin
            .flush()
            .await
            .map_err(|error| McpError::transport(error.to_string()))
    }
}

impl Transport for StdioTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Stdio
    }

    fn start(
        &self,
        inbound: UnboundedSender<Inbound>,
    ) -> TransportFuture<'_, Result<(), McpError>> {
        Box::pin(async move { self.spawn(inbound) })
    }

    fn send(&self, message: Value) -> TransportFuture<'_, Result<(), McpError>> {
        Box::pin(self.write(message))
    }

    fn close(&self) -> TransportFuture<'_, Result<(), McpError>> {
        Box::pin(self.shut())
    }

    fn sibling(&self) -> Option<Box<dyn Transport>> {
        Some(Box::new(Self {
            params: StdioParams {
                stderr: Stderr::Ignore,
                ..self.params.clone()
            },
            sibling: true,
            state: Mutex::new(State::New),
        }))
    }
}

/// Owns the child: kills it when told, and once it has exited and its
/// output is read, says the connection closed. (SDK: the process's `close`
/// event.)
async fn supervise(
    mut child: Child,
    reader: Option<JoinHandle<()>>,
    mut kill: mpsc::UnboundedReceiver<()>,
    exit: watch::Sender<bool>,
    inbound: UnboundedSender<Inbound>,
) {
    loop {
        tokio::select! {
            _ = child.wait() => break,
            Some(()) = kill.recv() => {
                let _ = child.start_kill();
            }
        }
    }
    exit.send_replace(true);
    if let Some(reader) = reader {
        let _ = reader.await;
    }
    let _ = inbound.send(Inbound::Closed);
}

/// Reads newline-delimited messages. A line that isn't JSON is skipped, as
/// the SDK's `ReadBuffer` skips it; JSON that isn't a JSON-RPC message is an
/// error, and reading goes on. Output beyond the buffer's limit ends the
/// connection.
async fn read(
    mut stdout: ChildStdout,
    inbound: UnboundedSender<Inbound>,
    kill: mpsc::UnboundedSender<()>,
) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = vec![0; 64 * 1024];
    loop {
        let read = match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        if buffer.len() + read > STDIO_MAX_BUFFER_BYTES {
            buffer.clear();
            let _ = inbound.send(Inbound::Error(format!(
                "ReadBuffer exceeded maximum size of {STDIO_MAX_BUFFER_BYTES} bytes"
            )));
            // The SDK closes the transport.
            let _ = kill.send(());
            return;
        }
        buffer.extend_from_slice(&chunk[..read]);
        while let Some(end) = buffer.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let line = &line[..line.len() - 1];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Some(event) = parse_line(line) {
                let _ = inbound.send(event);
            }
        }
    }
}

/// One line as what the client gets: a message, an error, or nothing.
pub(crate) fn parse_line(line: &[u8]) -> Option<Inbound> {
    let value: Value = serde_json::from_slice(line).ok()?;
    Some(match jsonrpc_problem(&value) {
        None => Inbound::Message(value),
        Some(problem) => Inbound::Error(problem),
    })
}

/// Why `value` isn't a JSON-RPC 2.0 message, if it isn't.
pub(crate) fn jsonrpc_problem(value: &Value) -> Option<String> {
    let Some(object) = value.as_object() else {
        return Some(format!("Invalid JSON-RPC message: {value}"));
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(format!("Invalid JSON-RPC message: {value}"));
    }
    let request = object.get("method").is_some_and(Value::is_string);
    let response = object.contains_key("id")
        && (object.contains_key("result") || object.contains_key("error"));
    (!request && !response).then(|| format!("Invalid JSON-RPC message: {value}"))
}
