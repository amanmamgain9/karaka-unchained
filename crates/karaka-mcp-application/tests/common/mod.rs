//! An in-process MCP server over HTTP, speaking the legacy handshake, and
//! stand-in agents. (Karaka's specs use the SDK's `createMcpHandler` on a
//! loopback `node:http` server.)

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use karaka_agent::{
    Agent, AgentOpenHooks, Agents, FrameListener, FrameListeners, FrameSubscription, InboxError,
    OpenError, WhenIdle,
};
use karaka_scope::{PresetId, ScopeChain};
use karaka_session::{CancelCause, SessionId, UserMessage};
use serde_json::{Value, json};

/// One request as the server read it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    /// Lower-cased header names and their values.
    pub headers: HashMap<String, String>,
    pub body: Value,
}

/// What the server answers, and what it was asked.
#[derive(Default)]
pub struct Fixture {
    /// The `tools/list` answer's tools.
    pub tools: Mutex<Vec<Value>>,
    /// Every request.
    pub seen: Mutex<Vec<Seen>>,
    /// The params of every `tools/call`.
    pub calls: Mutex<Vec<Value>>,
}

impl Fixture {
    pub fn calls(&self) -> Vec<Value> {
        self.calls.lock().unwrap().clone()
    }

    /// The headers of each request for `method`.
    pub fn headers_of(&self, method: &str) -> Vec<HashMap<String, String>> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.body["method"] == method)
            .map(|seen| seen.headers.clone())
            .collect()
    }
}

fn read_request(socket: &mut TcpStream) -> Option<(String, HashMap<String, String>, String)> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]).to_string();
            let mut lines = head.lines();
            let method = lines.next()?.split(' ').next()?.to_owned();
            let headers: HashMap<String, String> = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
                .collect();
            let length: usize = headers
                .get("content-length")
                .map_or(0, |value| value.parse().unwrap());
            while bytes.len() < end + 4 + length {
                let read = socket.read(&mut chunk).ok()?;
                if read == 0 {
                    return None;
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            let body = String::from_utf8_lossy(&bytes[end + 4..end + 4 + length]).to_string();
            return Some((method, headers, body));
        }
        let read = socket.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

fn respond(socket: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &str) {
    let mut response = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    ));
    let _ = socket.write_all(response.as_bytes());
}

fn answer(fixture: &Fixture, socket: &mut TcpStream) {
    let Some((method, headers, body)) = read_request(socket) else {
        return;
    };
    if method == "GET" {
        respond(socket, "405 Method Not Allowed", &[], "");
        return;
    }
    let message: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    fixture.seen.lock().unwrap().push(Seen {
        method,
        headers,
        body: message.clone(),
    });
    let id = message["id"].clone();
    let json_headers = [("content-type", "application/json")];
    let reply = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
    match message["method"].as_str().unwrap_or_default() {
        "server/discover" => respond(
            socket,
            "200 OK",
            &json_headers,
            &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } }).to_string(),
        ),
        "initialize" => respond(
            socket,
            "200 OK",
            &json_headers,
            &reply(json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "application-fixture", "version": "1" },
            })),
        ),
        "tools/list" => {
            let tools = fixture.tools.lock().unwrap().clone();
            respond(socket, "200 OK", &json_headers, &reply(json!({ "tools": tools })));
        }
        "tools/call" => {
            fixture.calls.lock().unwrap().push(message["params"].clone());
            respond(
                socket,
                "200 OK",
                &json_headers,
                &reply(json!({ "content": [{ "type": "text", "text": "accepted" }] })),
            );
        }
        _ => respond(socket, "202 Accepted", &[], ""),
    }
}

/// Serves `fixture` on a loopback port: its `/mcp` URL.
pub fn serve(fixture: Arc<Fixture>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for socket in listener.incoming() {
            let Ok(mut socket) = socket else { return };
            let fixture = Arc::clone(&fixture);
            thread::spawn(move || answer(&fixture, &mut socket));
        }
    });
    format!("http://{address}/mcp")
}

/// Answers every request with a 307 to `target`.
pub fn redirect(target: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for socket in listener.incoming() {
            let Ok(mut socket) = socket else { return };
            let _ = read_request(&mut socket);
            respond(
                &mut socket,
                "307 Temporary Redirect",
                &[("location", &target)],
                "",
            );
        }
    });
    format!("http://{address}/mcp")
}

/// An agent that does nothing, in an optional preset.
pub struct Stub {
    pub id: SessionId,
    pub preset: Option<PresetId>,
    frames: FrameListeners,
}

impl Agent for Stub {
    fn id(&self) -> &SessionId {
        &self.id
    }

    fn followup(&self, _: UserMessage) -> Result<(), InboxError> {
        Ok(())
    }

    fn steer(&self, _: UserMessage) -> Result<(), InboxError> {
        Ok(())
    }

    fn inject(&self, _: UserMessage) -> Result<(), InboxError> {
        Ok(())
    }

    fn cancel(&self, _: CancelCause) {}

    fn when_idle(&self) -> WhenIdle<'_> {
        Box::pin(async {})
    }

    fn subscribe(&self, listener: FrameListener) -> FrameSubscription {
        self.frames.subscribe(listener)
    }

    fn scope(&self) -> ScopeChain {
        ScopeChain::new(self.preset.clone(), Some(self.id.clone()))
    }
}

/// Stand-in agents, which tell the open hooks as the loop does.
#[derive(Default)]
pub struct Registry {
    pub live: Mutex<Vec<Arc<dyn Agent>>>,
    pub opened: Arc<AgentOpenHooks>,
}

impl Registry {
    /// Opens the agent of `id`, in `preset`.
    pub fn enter(&self, id: &str, preset: Option<&str>) -> Arc<dyn Agent> {
        let agent: Arc<dyn Agent> = Arc::new(Stub {
            id: SessionId::new(id),
            preset: preset.map(PresetId::new),
            frames: FrameListeners::new(),
        });
        self.live.lock().unwrap().push(Arc::clone(&agent));
        self.opened.opened(&agent);
        agent
    }
}

impl Agents for Registry {
    fn open(&self, id: SessionId) -> Result<Arc<dyn Agent>, OpenError> {
        Ok(self
            .get(&id)
            .unwrap_or_else(|| self.enter(id.as_str(), None)))
    }

    fn get(&self, id: &SessionId) -> Option<Arc<dyn Agent>> {
        self.live
            .lock()
            .unwrap()
            .iter()
            .find(|agent| agent.id() == id)
            .cloned()
    }

    fn list(&self) -> Vec<Arc<dyn Agent>> {
        self.live.lock().unwrap().clone()
    }
}
