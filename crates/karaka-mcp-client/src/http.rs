//! The Streamable HTTP transport: each message a POST to one endpoint, its
//! reply as JSON or as an SSE stream, and server-initiated messages on an
//! optional GET stream. (SDK: `StreamableHTTPClientTransport`,
//! `client/dist/index.mjs`; dsh: `createTransport`'s `streamable-http` arm,
//! with `requestInit.headers`.)
//!
//! Redirects are followed, as `fetch` follows them by default and the SDK
//! sets no redirect option; reqwest follows up to ten, and drops
//! `Authorization` when one leaves the origin, as `fetch` does.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use karaka_sse::Sse;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client as HttpClient, Response, StatusCode};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use crate::protocol::{ErrorKind, McpError, meta};
use crate::transport::{Inbound, Transport, TransportFuture, TransportKind};

/// The session id header. (SDK: `mcp-session-id`.)
const SESSION_ID: &str = "mcp-session-id";

/// The negotiated revision's header.
const PROTOCOL_VERSION: &str = "mcp-protocol-version";

/// How many times the GET stream is resumed after it drops. (SDK:
/// `maxRetries`, 2.)
const MAX_RECONNECTS: u32 = 2;

/// How to reach an HTTP server. (dsh: `StreamableHttpConfig`'s `url` and
/// `headers`.)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpParams {
    pub url: String,
    /// Sent with every request.
    pub headers: Vec<(String, String)>,
}

/// The Streamable HTTP transport. (SDK: `StreamableHTTPClientTransport`.)
pub struct HttpTransport {
    params: HttpParams,
    client: HttpClient,
    state: Arc<Mutex<State>>,
    /// Becomes `true` on close, which ends every stream.
    stop: watch::Sender<bool>,
}

#[derive(Default)]
struct State {
    inbound: Option<UnboundedSender<Inbound>>,
    session_id: Option<String>,
    protocol_version: Option<String>,
    closed: bool,
}

impl HttpTransport {
    pub fn new(params: HttpParams) -> Self {
        let client = HttpClient::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("the HTTP client fails to build only if TLS can't start");
        Self {
            params,
            client,
            state: Arc::default(),
            stop: watch::channel(false).0,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The session id the server gave `initialize`'s answer, if it gave one.
    pub fn session_id(&self) -> Option<String> {
        self.lock().session_id.clone()
    }

    /// The headers every request carries: the configured ones, the session
    /// id, and the negotiated revision.
    fn common_headers(&self, with_session: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.params.headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }
        let state = self.lock();
        if with_session && let Some(session) = &state.session_id {
            insert(&mut headers, SESSION_ID, session);
        }
        if let Some(version) = &state.protocol_version {
            insert(&mut headers, PROTOCOL_VERSION, version);
        }
        headers
    }

    async fn post(&self, message: Value) -> Result<(), McpError> {
        let inbound = {
            let state = self.lock();
            if state.closed {
                return Err(McpError::not_connected());
            }
            state.inbound.clone().ok_or_else(McpError::not_connected)?
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let is_initialize = method.as_deref() == Some("initialize");
        let is_request = method.is_some() && message.get("id").is_some();
        let mut headers = self.common_headers(!is_initialize);
        body_headers(&mut headers, &message);
        insert(&mut headers, CONTENT_TYPE.as_str(), "application/json");
        insert(
            &mut headers,
            ACCEPT.as_str(),
            "application/json, text/event-stream",
        );
        let response = self
            .client
            .post(&self.params.url)
            .headers(headers)
            .body(message.to_string())
            .send()
            .await
            .map_err(|error| McpError::transport(error.to_string()))?;
        let status = response.status();
        if is_initialize
            && status.is_success()
            && let Some(session) = header(&response, SESSION_ID)
        {
            self.lock().session_id = Some(session);
        }
        if status == StatusCode::ACCEPTED {
            let _ = response.bytes().await;
            if method.as_deref() == Some("notifications/initialized") {
                self.open_get_stream(inbound);
            }
            return Ok(());
        }
        if !status.is_success() {
            let status_text = status.canonical_reason().unwrap_or_default().to_owned();
            let text = response.text().await.unwrap_or_default();
            // A modern request's JSON-RPC error may come back as a 400.
            if status == StatusCode::BAD_REQUEST
                && let Ok(error) = serde_json::from_str::<Value>(&text)
                && error.get("error").is_some()
                && error.get("id") == message.get("id")
            {
                let _ = inbound.send(Inbound::Message(error));
                return Ok(());
            }
            return Err(McpError {
                kind: ErrorKind::Http(status.as_u16()),
                message: format!("Error POSTing to endpoint: {text}"),
                data: Some(
                    json!({ "status": status.as_u16(), "statusText": status_text, "text": text }),
                ),
            });
        }
        if !is_request {
            let _ = response.bytes().await;
            return Ok(());
        }
        let content_type = header(&response, CONTENT_TYPE.as_str()).unwrap_or_default();
        if content_type.contains("text/event-stream") {
            tokio::spawn(read_stream(response, inbound, self.stop.subscribe()));
            return Ok(());
        }
        if content_type.contains("application/json") {
            let body = response
                .bytes()
                .await
                .map_err(|error| McpError::transport(error.to_string()))?;
            match serde_json::from_slice::<Value>(&body) {
                Ok(Value::Array(messages)) => {
                    for message in messages {
                        let _ = inbound.send(Inbound::Message(message));
                    }
                }
                Ok(message) => {
                    let _ = inbound.send(Inbound::Message(message));
                }
                Err(error) => {
                    let _ = inbound.send(Inbound::Error(error.to_string()));
                }
            }
            return Ok(());
        }
        Err(McpError::other(format!(
            "Unexpected content type: {content_type}"
        )))
    }

    /// Opens the GET stream for messages the server sends unasked, after
    /// the handshake, and resumes it with `last-event-id` if it drops, at
    /// most twice. A 405 means the server offers none. (SDK:
    /// `_startOrAuthSse` and `_scheduleReconnection`.)
    fn open_get_stream(&self, inbound: UnboundedSender<Inbound>) {
        let client = self.client.clone();
        let url = self.params.url.clone();
        let headers = self.common_headers(true);
        let mut stop = self.stop.subscribe();
        tokio::spawn(async move {
            let mut last_event_id: Option<String> = None;
            let mut attempt: u32 = 0;
            let mut retry_ms: Option<u64> = None;
            loop {
                let mut request = client
                    .get(&url)
                    .headers(headers.clone())
                    .header(ACCEPT, "text/event-stream");
                if let Some(id) = &last_event_id {
                    request = request.header("last-event-id", id.as_str());
                }
                let response = tokio::select! {
                    response = request.send() => response,
                    _ = stop.wait_for(|stop| *stop) => return,
                };
                match response {
                    Ok(response) if response.status() == StatusCode::METHOD_NOT_ALLOWED => return,
                    Ok(response) if !response.status().is_success() => {
                        let text = response.status().canonical_reason().unwrap_or_default();
                        let _ = inbound
                            .send(Inbound::Error(format!("Failed to open SSE stream: {text}")));
                        return;
                    }
                    Ok(response) => {
                        let read = read_events(response, &inbound, stop.clone()).await;
                        if read.stopped {
                            return;
                        }
                        if read.last_event_id.is_some() {
                            last_event_id = read.last_event_id;
                        }
                        if read.retry_ms.is_some() {
                            retry_ms = read.retry_ms;
                        }
                        if let Some(error) = read.error {
                            let _ = inbound
                                .send(Inbound::Error(format!("SSE stream disconnected: {error}")));
                        }
                    }
                    Err(error) => {
                        let _ = inbound.send(Inbound::Error(format!(
                            "Failed to reconnect SSE stream: {error}"
                        )));
                    }
                }
                if attempt >= MAX_RECONNECTS {
                    let _ = inbound.send(Inbound::Error(format!(
                        "Maximum reconnection attempts ({MAX_RECONNECTS}) exceeded."
                    )));
                    return;
                }
                let delay = retry_ms.unwrap_or_else(|| {
                    (1000.0 * 1.5_f64.powi(i32::try_from(attempt).unwrap_or(i32::MAX)))
                        .min(30_000.0) as u64
                });
                attempt += 1;
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(delay)) => {}
                    _ = stop.wait_for(|stop| *stop) => return,
                }
            }
        });
    }
}

impl Transport for HttpTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Http
    }

    fn start(
        &self,
        inbound: UnboundedSender<Inbound>,
    ) -> TransportFuture<'_, Result<(), McpError>> {
        Box::pin(async move {
            let mut state = self.lock();
            if state.inbound.is_some() || state.closed {
                return Err(McpError::transport(
                    "StreamableHTTPClientTransport already started! If using Client class, note that connect() calls start() automatically.",
                ));
            }
            state.inbound = Some(inbound);
            Ok(())
        })
    }

    fn send(&self, message: Value) -> TransportFuture<'_, Result<(), McpError>> {
        Box::pin(self.post(message))
    }

    fn set_protocol_version(&self, version: &str) {
        self.lock().protocol_version = Some(version.to_owned());
    }

    /// Ends every stream and says the connection closed. As the SDK's, it
    /// sends no DELETE: ending the session is `terminateSession`, which
    /// isn't ported.
    fn close(&self) -> TransportFuture<'_, Result<(), McpError>> {
        Box::pin(async move {
            let inbound = {
                let mut state = self.lock();
                if state.closed {
                    return Ok(());
                }
                state.closed = true;
                state.inbound.take()
            };
            self.stop.send_replace(true);
            if let Some(inbound) = inbound {
                let _ = inbound.send(Inbound::Closed);
            }
            Ok(())
        })
    }
}

fn insert(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

fn header(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The headers a modern message's envelope implies: its revision, its
/// method, and for a call or a read the name it is about. (SDK:
/// `_applyBodyDerivedHeaders`.)
fn body_headers(headers: &mut HeaderMap, message: &Value) {
    let Some(version) = message
        .pointer("/params/_meta")
        .and_then(|meta| meta.get(meta::PROTOCOL_VERSION))
        .and_then(Value::as_str)
    else {
        return;
    };
    insert(headers, PROTOCOL_VERSION, version);
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !method.is_empty() {
        insert(headers, "mcp-method", method);
    }
    let name = match method {
        "tools/call" | "prompts/get" => message.pointer("/params/name"),
        "resources/read" => message.pointer("/params/uri"),
        _ => None,
    };
    if let Some(name) = name.and_then(Value::as_str) {
        insert(headers, "mcp-name", name);
    }
}

/// What reading one SSE stream came to.
struct Read {
    stopped: bool,
    last_event_id: Option<String>,
    retry_ms: Option<u64>,
    error: Option<String>,
}

/// Reads one POST's SSE reply to its end.
async fn read_stream(
    response: Response,
    inbound: UnboundedSender<Inbound>,
    stop: watch::Receiver<bool>,
) {
    let read = read_events(response, &inbound, stop).await;
    if let Some(error) = read.error {
        let _ = inbound.send(Inbound::Error(format!("SSE stream disconnected: {error}")));
    }
}

/// Puts each message event of `response` on `inbound`: unnamed events and
/// `message` ones; others are ignored, as the SDK ignores them.
async fn read_events(
    mut response: Response,
    inbound: &UnboundedSender<Inbound>,
    mut stop: watch::Receiver<bool>,
) -> Read {
    let mut sse = Sse::default();
    let mut read = Read {
        stopped: false,
        last_event_id: None,
        retry_ms: None,
        error: None,
    };
    loop {
        let chunk = tokio::select! {
            chunk = response.chunk() => chunk,
            _ = stop.wait_for(|stop| *stop) => {
                read.stopped = true;
                return read;
            }
        };
        match chunk {
            Ok(Some(bytes)) => sse.push(&bytes),
            Ok(None) => return read,
            Err(error) => {
                read.error = Some(error.to_string());
                return read;
            }
        }
        if let Some(retry) = sse.take_retry() {
            read.retry_ms = Some(retry);
        }
        while let Some(event) = sse.next_message() {
            if let Some(id) = event.id {
                read.last_event_id = Some(id);
            }
            if event.event.as_deref().is_some_and(|name| name != "message") {
                continue;
            }
            match serde_json::from_str::<Value>(&event.data) {
                Ok(message) => {
                    let _ = inbound.send(Inbound::Message(message));
                }
                Err(error) => {
                    let _ = inbound.send(Inbound::Error(error.to_string()));
                }
            }
        }
    }
}
