//! Browser access: JWT-authenticated application methods over HTTP, one
//! WebSocket per browser for interactions and chat follows, and approvals
//! and questions of browser-reachable chats delivered to their owner's
//! browsers. (Karaka: `transport-http/src/browser-routes.ts` and
//! `browser-socket.ts`.)

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    AUTHORIZATION, CACHE_CONTROL, CONNECTION, CONTENT_TYPE, HeaderName, ORIGIN,
    SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE, VARY,
};
use http::{HeaderValue, Method, StatusCode};
use karaka_browser_auth::{BrowserAuthentication, BrowserCaller};
use karaka_identity::{ApplicationOwner, KarakaIdentity, same_owner};
use karaka_server_auth::bearer_token;
use karaka_session::{Entry, SessionHeader, SessionId, SessionStore};
use karaka_startup::{KarakaStartup, STARTING_BODY, STARTING_CONTENT_TYPE};
use karaka_user_approval::{
    ApprovalAnswerer, ApprovalFuture, ApprovalNext, ApprovalOutcome, ApprovalRequest,
};
use karaka_user_questions::{
    AnswerFuture, AnswerNext, AskUserQuestionAnswer, AskUserQuestionAnswerItem, QuestionAnswerer,
    QuestionError, QuestionRequest,
};
use karaka_webserver::{
    Request, Response, TokioIo, Upgraded, Upgrades, body, on_upgrade, pathname,
};
use serde_json::{Map, Value, json};
use tokio::io::ReadHalf;
use tokio::sync::{mpsc, oneshot};

use crate::controller::{Address, ApplicationChats, Follow, FollowFrame};
use crate::error::AppError;
use crate::ids::random_uuid;
use crate::routes::{json_response, read_object};
use crate::wire;
use crate::ws::{self, Incoming, ReadError};

/// The application methods a browser may call. (Karaka:
/// `APPLICATION_REMOTE_METHODS`.)
pub const BROWSER_METHODS: [&str; 6] = [
    "applicationAgents",
    "applicationCreate",
    "applicationPrompt",
    "applicationHistory",
    "applicationFollow",
    "applicationCancel",
];

/// The interactions a browser may answer. (Karaka: `browserEvents`.)
pub const BROWSER_EVENTS: [&str; 2] = ["approval/request", "user-questions/request"];

/// The socket's application close codes. (Karaka: `SOCKET_CLOSE`.)
pub const CLOSE_PROTOCOL: u16 = 4400;
pub const CLOSE_UNAUTHORIZED: u16 = 4401;
pub const CLOSE_FORBIDDEN: u16 = 4403;
pub const CLOSE_FAILED: u16 = 4500;

/// (Karaka: `Config`'s browser fields.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserConfig {
    pub path: String,
    pub origins: Vec<String>,
    pub agent_ids: Vec<String>,
    pub methods: Vec<String>,
    pub events: Vec<String>,
    pub heartbeat_ms: u64,
}

impl BrowserConfig {
    /// A config with Karaka's defaults: every method and event, a 15 s
    /// heartbeat.
    pub fn new(path: impl Into<String>, origins: Vec<String>, agent_ids: Vec<String>) -> Self {
        Self {
            path: path.into(),
            origins,
            agent_ids,
            methods: BROWSER_METHODS.map(str::to_owned).to_vec(),
            events: BROWSER_EVENTS.map(str::to_owned).to_vec(),
            heartbeat_ms: 15_000,
        }
    }

    /// Checks it against the backend prefix `base`, in Karaka's words.
    ///
    /// # Errors
    ///
    /// No origins, no or blank presets, a bad path, paths that overlap, an
    /// unknown method or event, or a zero heartbeat.
    pub fn check(&self, base: &str) -> Result<(), String> {
        if self.origins.is_empty() {
            return Err("Browser transport requires explicit origins".into());
        }
        if self.agent_ids.is_empty() || self.agent_ids.iter().any(|id| id.trim().is_empty()) {
            return Err("Browser transport requires explicit Agent presets".into());
        }
        crate::normalize_base(&self.path)?;
        let browser = &self.path;
        if browser == base
            || browser.starts_with(&format!("{base}/"))
            || base.starts_with(&format!("{browser}/"))
        {
            return Err("Browser and backend paths must be disjoint".into());
        }
        if let Some(method) = self
            .methods
            .iter()
            .find(|method| !BROWSER_METHODS.contains(&method.as_str()))
        {
            return Err(format!("unknown browser method \"{method}\""));
        }
        if let Some(event) = self
            .events
            .iter()
            .find(|event| !BROWSER_EVENTS.contains(&event.as_str()))
        {
            return Err(format!("unknown browser event \"{event}\""));
        }
        if self.heartbeat_ms == 0 {
            return Err("browserHeartbeatMs must be a positive safe integer".into());
        }
        Ok(())
    }
}

/// How a pending interaction settled.
enum Outcome {
    /// Every recipient delegated: the chain goes on.
    Next,
    Result(Value),
}

struct Channel {
    id: String,
    owner: ApplicationOwner,
    chat: SessionId,
    push: mpsc::UnboundedSender<Value>,
}

struct Pending {
    id: String,
    chat: SessionId,
    owner: ApplicationOwner,
    event: &'static str,
    request: Value,
    recipients: HashSet<String>,
    resolve: Option<oneshot::Sender<Outcome>>,
}

impl Pending {
    fn frame(&self) -> Value {
        json!({
            "type": "interaction",
            "eventId": self.id,
            "chatId": self.chat.as_str(),
            "event": self.event,
            "request": self.request,
        })
    }
}

/// What a browser endpoint shares: its channels and pending interactions.
pub struct Browser {
    config: BrowserConfig,
    max_body_bytes: usize,
    chats: Arc<ApplicationChats>,
    auth: Arc<BrowserAuthentication>,
    identity: Arc<KarakaIdentity>,
    store: Arc<SessionStore>,
    startup: Arc<KarakaStartup>,
    channels: Mutex<Vec<Channel>>,
    pending: Mutex<Vec<Pending>>,
    me: std::sync::Weak<Browser>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// A failure as a browser sees it, and its status. (Karaka: `failure`.)
pub fn failure(error: &AppError) -> (u16, Value) {
    let code = error.code.as_deref();
    let denied = matches!(code, Some("forbidden" | "CHAT_FORBIDDEN"));
    let bad = code == Some("BAD_REQUEST");
    let (status, code, message) = if denied {
        (403, "gateway/forbidden", "Chat access is forbidden")
    } else if bad {
        (400, "gateway/bad-request", "Invalid request")
    } else {
        (500, "gateway/internal", "Browser operation failed")
    };
    (
        status,
        json!({ "ok": false, "error": { "code": code, "message": message, "details": {} } }),
    )
}

/// The header as dsh's in-memory `SessionHeader`.
fn header_json(header: &SessionHeader) -> Value {
    let mut value = Map::new();
    value.insert(
        "version".into(),
        json!(karaka_session::SESSION_FORMAT_VERSION),
    );
    value.insert("id".into(), json!(header.id.as_str()));
    value.insert("createdAt".into(), json!(header.created_at));
    if let Some(cwd) = &header.cwd {
        value.insert("cwd".into(), json!(cwd));
    }
    if let Some(parent) = &header.parent_session {
        value.insert("parentSession".into(), json!(parent.as_str()));
    }
    value.insert("isSeeded".into(), json!(header.is_seeded));
    if header.origin.is_some() {
        value.insert("origin".into(), json!("subagent"));
    }
    value.insert("delegationDepth".into(), json!(header.delegation_depth));
    if let Some(preset) = &header.agent_preset {
        value.insert("agentPreset".into(), json!(preset));
    }
    Value::Object(value)
}

fn event_json(entry: &Entry) -> Value {
    serde_json::to_value(entry).unwrap_or(Value::Null)
}

/// A follow's snapshot frame. (Karaka: `FollowFrame`'s `snapshot`.)
fn snapshot_json(follow: &Follow) -> Value {
    json!({
        "type": "snapshot",
        "header": header_json(&follow.header),
        "hasMore": false,
        "projections": { "asOfSeq": follow.cursor, "values": {} },
        "cursor": follow.cursor,
        "records": follow.records.iter().map(|entry| json!({ "type": "event", "event": event_json(entry) })).collect::<Vec<_>>(),
    })
}

fn frame_json(frame: &FollowFrame) -> Value {
    match frame {
        FollowFrame::Event(entry) => json!({ "type": "event", "event": event_json(entry) }),
        FollowFrame::TextDelta { cursor, text } => {
            json!({ "type": "text-delta", "cursor": cursor, "text": text })
        }
    }
}

fn invalid() -> AppError {
    AppError::bad_request("request body does not match its schema")
}

fn strict<'a>(value: &'a Value, keys: &[&str]) -> Result<&'a Map<String, Value>, AppError> {
    match value {
        Value::Object(map) if map.keys().all(|key| keys.contains(&key.as_str())) => Ok(map),
        _ => Err(invalid()),
    }
}

fn text(map: &Map<String, Value>, key: &str) -> Result<String, AppError> {
    match map.get(key) {
        Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
        _ => Err(invalid()),
    }
}

fn any_string(map: &Map<String, Value>, key: &str) -> Result<String, AppError> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(invalid)
}

/// `{ chatId }`. (Karaka: `address`.)
fn chat_of(value: &Value) -> Result<SessionId, AppError> {
    Ok(SessionId::new(text(strict(value, &["chatId"])?, "chatId")?))
}

/// An answer's value. (Karaka: `answer`.)
fn answer_of(value: &Value) -> Result<AskUserQuestionAnswer, AppError> {
    let map = strict(value, &["answers"])?;
    let Some(Value::Array(items)) = map.get("answers") else {
        return Err(invalid());
    };
    let answers = items
        .iter()
        .map(|item| {
            let item = strict(item, &["id", "selected", "custom"])?;
            let Some(Value::Array(labels)) = item.get("selected") else {
                return Err(invalid());
            };
            let selected = labels
                .iter()
                .map(|label| label.as_str().map(str::to_owned).ok_or_else(invalid))
                .collect::<Result<Vec<_>, _>>()?;
            let custom = match item.get("custom") {
                None => None,
                Some(Value::String(custom)) => Some(custom.clone()),
                Some(_) => return Err(invalid()),
            };
            Ok(AskUserQuestionAnswerItem {
                id: any_string(item, "id")?,
                selected,
                custom,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    Ok(AskUserQuestionAnswer { answers })
}

/// A cancelled interaction. (Karaka: `Interaction cancelled`.)
#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Interaction cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Takes an interaction out, and tells its chat's channels, however its
/// wait ends.
struct Withdraw<'a> {
    browser: &'a Browser,
    id: String,
    chat: SessionId,
    owner: ApplicationOwner,
}

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        lock(&self.browser.pending).retain(|pending| pending.id != self.id);
        let cancel = json!({ "type": "cancel", "eventId": self.id });
        for channel in lock(&self.browser.channels).iter() {
            if channel.chat == self.chat && same_owner(&channel.owner, &self.owner) {
                let _ = channel.push.send(cancel.clone());
            }
        }
    }
}

impl Browser {
    pub fn new(
        config: BrowserConfig,
        max_body_bytes: usize,
        chats: Arc<ApplicationChats>,
        auth: Arc<BrowserAuthentication>,
        identity: Arc<KarakaIdentity>,
        store: Arc<SessionStore>,
        startup: Arc<KarakaStartup>,
    ) -> Arc<Self> {
        chats.set_browser_presets(config.agent_ids.clone());
        Arc::new_cyclic(|me| Self {
            config,
            max_body_bytes,
            chats,
            auth,
            identity,
            store,
            startup,
            channels: Mutex::default(),
            pending: Mutex::default(),
            me: me.clone(),
        })
    }

    pub fn config(&self) -> &BrowserConfig {
        &self.config
    }

    fn allows_method(&self, method: &str) -> bool {
        self.config.methods.iter().any(|known| known == method)
    }

    /// Whether a session is a root of an allowed preset. (Karaka:
    /// `acceptsSession`.)
    fn accepts(&self, header: &SessionHeader) -> bool {
        header.parent_session.is_none()
            && header
                .agent_preset
                .as_ref()
                .is_some_and(|preset| self.config.agent_ids.contains(preset))
    }

    /// Offers an interaction to the owner's channels on its chat, and
    /// waits for one to answer, or all to delegate. (Karaka: `ask`.)
    async fn ask(
        &self,
        event: &'static str,
        chat: &SessionId,
        owner: &ApplicationOwner,
        request: Value,
        signal: Option<karaka_fs::AbortSignal>,
    ) -> Result<Outcome, Cancelled> {
        let recipients: HashSet<String> = lock(&self.channels)
            .iter()
            .filter(|channel| channel.chat == *chat && same_owner(&channel.owner, owner))
            .map(|channel| channel.id.clone())
            .collect();
        if recipients.is_empty() {
            return Ok(Outcome::Next);
        }
        let (resolve, settled) = oneshot::channel();
        let pending = Pending {
            id: random_uuid(),
            chat: chat.clone(),
            owner: owner.clone(),
            event,
            request,
            recipients,
            resolve: Some(resolve),
        };
        let frame = pending.frame();
        let _withdraw = Withdraw {
            browser: self,
            id: pending.id.clone(),
            chat: chat.clone(),
            owner: owner.clone(),
        };
        let recipients = pending.recipients.clone();
        lock(&self.pending).push(pending);
        for channel in lock(&self.channels)
            .iter()
            .filter(|channel| recipients.contains(&channel.id))
        {
            let _ = channel.push.send(frame.clone());
        }
        if signal
            .as_ref()
            .is_some_and(karaka_fs::AbortSignal::is_aborted)
        {
            return Err(Cancelled);
        }
        tokio::select! {
            outcome = settled => outcome.map_err(|_| Cancelled),
            () = async {
                match &signal {
                    Some(signal) => signal.wait().await,
                    None => std::future::pending().await,
                }
            } => Err(Cancelled),
        }
    }

    /// Registers a recipient: it gets `ready`, then every interaction
    /// pending on its chat for its owner. (Karaka: `join`.)
    fn join(
        &self,
        caller: &BrowserCaller,
        push: mpsc::UnboundedSender<Value>,
        extra: Map<String, Value>,
    ) -> String {
        let id = random_uuid();
        let home = std::env::var("HOME").unwrap_or_default();
        let mut ready = json!({ "type": "ready", "clientId": id, "host": { "home": home } });
        for (key, value) in extra {
            ready[key] = value;
        }
        let _ = push.send(ready);
        let mut pending = lock(&self.pending);
        for item in pending.iter_mut() {
            if item.chat == caller.chat_id && same_owner(&item.owner, &caller.owner) {
                item.recipients.insert(id.clone());
                let _ = push.send(item.frame());
            }
        }
        drop(pending);
        lock(&self.channels).push(Channel {
            id: id.clone(),
            owner: caller.owner.clone(),
            chat: caller.chat_id.clone(),
            push,
        });
        id
    }

    /// Unregisters a recipient. A pending interaction stays, and replays
    /// when the same owner and chat reconnect.
    fn leave(&self, id: &str) {
        lock(&self.channels).retain(|channel| channel.id != id);
        for item in lock(&self.pending).iter_mut() {
            item.recipients.remove(id);
        }
    }

    fn address(caller: &BrowserCaller, chat: SessionId) -> Address {
        Address {
            chat_id: chat,
            owner: caller.owner.clone(),
            browser: Some(caller.clone()),
        }
    }

    /// Checks the caller's own chat. (Karaka: `admit`.)
    async fn admit(&self, caller: &BrowserCaller) -> Result<(), AppError> {
        self.chats
            .authorize_browser(&Self::address(caller, caller.chat_id.clone()), None, caller)
            .await?;
        Ok(())
    }

    fn with_cors(mut response: Response, origin: &HeaderValue) -> Response {
        let headers = response.headers_mut();
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
        headers.insert(VARY, HeaderValue::from_static("Origin"));
        response
    }

    /// One HTTP request under the browser prefix. (Karaka: the prefix
    /// route's handler.)
    pub async fn handle(self: Arc<Self>, request: Request) -> Response {
        if !self.startup.is_ready() {
            let mut response = Response::new(body::full(STARTING_BODY));
            *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            response.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static(STARTING_CONTENT_TYPE),
            );
            return response;
        }
        let origin = request.headers().get(ORIGIN).cloned();
        let Some(origin) = origin.filter(|origin| {
            origin
                .to_str()
                .is_ok_and(|origin| self.config.origins.iter().any(|known| known == origin))
        }) else {
            return json_response(
                403,
                &json!({ "code": "ORIGIN_FORBIDDEN", "message": "Browser origin is forbidden" }),
            );
        };
        let response = self.handle_allowed(request).await;
        Self::with_cors(response, &origin)
    }

    async fn handle_allowed(&self, request: Request) -> Response {
        if request.method() == Method::OPTIONS {
            let mut response = Response::new(body::empty());
            *response.status_mut() = StatusCode::NO_CONTENT;
            let headers = response.headers_mut();
            headers.insert(
                ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("POST"),
            );
            headers.insert(
                ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("authorization,content-type"),
            );
            return response;
        }
        let caller = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_token)
            .and_then(|token| self.auth.authenticate(token));
        let Some(caller) = caller else {
            return json_response(
                401,
                &json!({ "code": "UNAUTHORIZED", "message": "Invalid browser credential" }),
            );
        };
        let path = pathname(request.uri());
        let method = path
            .get(self.config.path.len() + 1..)
            .unwrap_or_default()
            .to_owned();
        if request.method() != Method::POST {
            return json_response(
                405,
                &json!({ "code": "METHOD_FORBIDDEN", "message": "POST required" }),
            );
        }
        match self.operate(&caller, &method, request).await {
            Ok(response) => response,
            Err(error) => {
                let (status, envelope) = failure(&error);
                json_response(status, &envelope)
            }
        }
    }

    async fn operate(
        &self,
        caller: &BrowserCaller,
        method: &str,
        request: Request,
    ) -> Result<Response, AppError> {
        let body = read_object(request.into_body(), self.max_body_bytes).await?;
        if method == "events" {
            strict(&body, &[])?;
            self.admit(caller).await?;
            return Ok(self.events(caller));
        }
        if method == "result" {
            self.result(caller, &body).await?;
            return Ok(json_response(200, &json!({ "ok": true, "value": null })));
        }
        if !BROWSER_METHODS.contains(&method) || !self.allows_method(method) {
            return Ok(json_response(
                403,
                &json!({ "code": "METHOD_FORBIDDEN", "message": "Browser method is unavailable" }),
            ));
        }
        let chats = &self.chats;
        let value = match method {
            "applicationAgents" => {
                strict(&body, &[])?;
                let agents = chats.list_agents();
                Value::Array(
                    agents
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|agent| {
                            agent["id"].as_str().is_some_and(|id| {
                                self.config.agent_ids.iter().any(|known| known == id)
                            })
                        })
                        .cloned()
                        .collect(),
                )
            }
            "applicationCreate" => {
                let map = strict(&body, &["agentId", "chatId"])?;
                let agent = text(map, "agentId")?;
                let chat = SessionId::new(text(map, "chatId")?);
                chats.create(&Self::address(caller, chat), &agent).await?
            }
            "applicationPrompt" => {
                let map = strict(&body, &["requestId", "content", "chatId"])?;
                let chat = SessionId::new(text(map, "chatId")?);
                let mut backend = map.clone();
                backend.remove("chatId");
                backend.insert("tenantId".into(), json!("-"));
                backend.insert("userId".into(), json!("-"));
                let prompt = wire::Prompt::parse(&Value::Object(backend))?;
                chats
                    .prompt(
                        &Self::address(caller, chat),
                        &prompt.request_id,
                        &prompt.content,
                    )
                    .await?
            }
            "applicationHistory" => {
                let chat = chat_of(&body)?;
                let events = chats.events(&Self::address(caller, chat)).await?;
                Value::Array(events.iter().map(event_json).collect())
            }
            "applicationCancel" => {
                let chat = chat_of(&body)?;
                chats.cancel(&Self::address(caller, chat)).await?
            }
            _ => {
                let chat = chat_of(&body)?;
                return self.follow_over_http(caller, chat).await;
            }
        };
        Ok(json_response(200, &json!({ "ok": true, "value": value })))
    }

    fn sse_response(stream_body: karaka_webserver::Body) -> Response {
        let mut response = Response::new(stream_body);
        let headers = response.headers_mut();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        headers.insert(
            CACHE_CONTROL,
            HeaderValue::from_static("no-cache, no-transform"),
        );
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        response
    }

    /// `events`: an SSE channel of interactions. (Karaka: `eventChannel`.)
    fn events(&self, caller: &BrowserCaller) -> Response {
        let (push, mut received) = mpsc::unbounded_channel::<Value>();
        let id = self.join(caller, push, Map::new());
        let (sender, stream_body) = body::channel(16);
        let browser = self.self_arc();
        let expires = caller.expires_at;
        tokio::spawn(async move {
            let deadline =
                tokio::time::sleep(Duration::from_millis(expires.saturating_sub(now_ms())));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    event = received.recv() => {
                        let Some(event) = event else { break };
                        if sender.send(Bytes::from(wire::sse(&event))).await.is_err() {
                            break;
                        }
                    }
                    () = sender.closed() => break,
                    () = &mut deadline => break,
                }
            }
            if let Some(browser) = browser {
                browser.leave(&id);
            }
        });
        Self::sse_response(stream_body)
    }

    /// The `Arc` this browser lives in, for tasks that outlive a request.
    fn self_arc(&self) -> Option<Arc<Self>> {
        self.me.upgrade()
    }

    /// `result`: a recipient's answer, or its delegation. (Karaka: the
    /// `result` route.)
    async fn result(&self, caller: &BrowserCaller, body: &Value) -> Result<(), AppError> {
        let map = strict(body, &["clientId", "eventId", "outcome"])?;
        let client = any_string(map, "clientId")?;
        let event = any_string(map, "eventId")?;
        let outcome = map.get("outcome").ok_or_else(invalid)?;
        let delegate = match outcome.get("kind").and_then(Value::as_str) {
            Some("next") => {
                strict(outcome, &["kind"])?;
                None
            }
            Some("result") => Some(
                strict(outcome, &["kind", "value"])?
                    .get("value")
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
            _ => return Err(invalid()),
        };
        let unavailable = || {
            AppError::coded(
                "CHAT_FORBIDDEN",
                "Interaction is unavailable to this caller",
            )
        };
        let recipient_ok = |browser: &Self| -> Result<&'static str, AppError> {
            let channels = lock(&browser.channels);
            let channel = channels.iter().find(|channel| channel.id == client);
            let pending = lock(&browser.pending);
            let item = pending.iter().find(|item| item.id == event);
            match (channel, item) {
                (Some(channel), Some(item))
                    if same_owner(&channel.owner, &caller.owner)
                        && channel.chat == caller.chat_id
                        && item.chat == caller.chat_id
                        && same_owner(&item.owner, &caller.owner)
                        && item.recipients.contains(&channel.id) =>
                {
                    Ok(item.event)
                }
                _ => Err(unavailable()),
            }
        };
        recipient_ok(self)?;
        let header = self
            .chats
            .authorize_browser(&Self::address(caller, caller.chat_id.clone()), None, caller)
            .await?;
        let kind = recipient_ok(self)?;
        if header.is_none() {
            return Err(unavailable());
        }
        let mut pending = lock(&self.pending);
        let index = pending
            .iter()
            .position(|item| item.id == event)
            .ok_or_else(unavailable)?;
        match delegate {
            Some(value) => {
                let value = if kind == "approval/request" {
                    let outcome: ApprovalOutcome =
                        serde_json::from_value(value).map_err(|_| invalid())?;
                    serde_json::to_value(outcome).unwrap_or(Value::Null)
                } else {
                    serde_json::to_value(answer_of(&value)?).unwrap_or(Value::Null)
                };
                let mut item = pending.remove(index);
                if let Some(resolve) = item.resolve.take() {
                    let _ = resolve.send(Outcome::Result(value));
                }
            }
            None => {
                let item = &mut pending[index];
                item.recipients.remove(&client);
                if item.recipients.is_empty() {
                    let mut item = pending.remove(index);
                    if let Some(resolve) = item.resolve.take() {
                        let _ = resolve.send(Outcome::Next);
                    }
                }
            }
        }
        Ok(())
    }

    /// `applicationFollow` over HTTP: `{ ok, value }` frames, the
    /// snapshot first. (Karaka: the follow branch of the route.)
    async fn follow_over_http(
        &self,
        caller: &BrowserCaller,
        chat: SessionId,
    ) -> Result<Response, AppError> {
        let mut follow = self.chats.follow(&Self::address(caller, chat)).await?;
        let (sender, stream_body) = body::channel(16);
        let first = json!({ "ok": true, "value": snapshot_json(&follow) });
        let expires = caller.expires_at;
        tokio::spawn(async move {
            if sender.send(Bytes::from(wire::sse(&first))).await.is_err() {
                return;
            }
            let deadline =
                tokio::time::sleep(Duration::from_millis(expires.saturating_sub(now_ms())));
            tokio::pin!(deadline);
            loop {
                let next = tokio::select! {
                    next = follow.next() => next,
                    () = sender.closed() => return,
                    () = &mut deadline => return,
                };
                let envelope = match next {
                    Ok(Some(frame)) => json!({ "ok": true, "value": frame_json(&frame) }),
                    Ok(None) => return,
                    Err(error) => {
                        let _ = sender
                            .send(Bytes::from(wire::sse(&failure(&error).1)))
                            .await;
                        return;
                    }
                };
                if sender
                    .send(Bytes::from(wire::sse(&envelope)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        Ok(Self::sse_response(stream_body))
    }
}

impl QuestionAnswerer for Browser {
    /// A question from a browser-reachable chat with an owner goes to
    /// that owner's browsers on the chat. (Karaka: the prepended
    /// `user-questions/request` listener.)
    fn answer<'a>(
        &'a self,
        request: &'a QuestionRequest,
        next: AnswerNext<'a>,
    ) -> AnswerFuture<'a> {
        Box::pin(async move {
            let Some(agent) = &request.agent else {
                return next.run().await;
            };
            let Some(session) = self.store.get(agent.id()) else {
                return next.run().await;
            };
            if !self.accepts(session.header()) {
                return next.run().await;
            }
            let Some(owner) = self
                .identity
                .owner_of_session(&session)
                .await
                .map_err(QuestionError::other)?
            else {
                return next.run().await;
            };
            let asked = json!({
                "agent": { "id": agent.id().as_str() },
                "questions": serde_json::to_value(&request.questions).unwrap_or(Value::Null),
            });
            match self
                .ask(
                    "user-questions/request",
                    agent.id(),
                    &owner,
                    asked,
                    request.signal.clone(),
                )
                .await
                .map_err(QuestionError::other)?
            {
                Outcome::Next => next.run().await,
                Outcome::Result(value) => {
                    serde_json::from_value(value).map_err(QuestionError::other)
                }
            }
        })
    }
}

impl ApprovalAnswerer for Browser {
    /// An approval from a browser-reachable chat with an owner goes to that
    /// owner's browsers on the chat. (Karaka: the prepended
    /// `approval/request` listener.)
    fn answer<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        next: ApprovalNext<'a>,
    ) -> ApprovalFuture<'a> {
        Box::pin(async move {
            let agent = &request.agent;
            let Some(session) = self.store.get(agent.id()) else {
                return next.run().await;
            };
            if !self.accepts(session.header()) {
                return next.run().await;
            }
            let Some(owner) = self.identity.owner_of_session(&session).await? else {
                return next.run().await;
            };
            let mut asked =
                json!({ "agent": { "id": agent.id().as_str() }, "toolName": request.tool_name });
            if let Some(reason) = &request.reason {
                asked["reason"] = json!(reason);
            }
            if let Some(call) = &request.call_id {
                asked["callId"] = json!(call);
            }
            match self
                .ask(
                    "approval/request",
                    agent.id(),
                    &owner,
                    asked,
                    request.signal.clone(),
                )
                .await?
            {
                Outcome::Next => next.run().await,
                Outcome::Result(value) => Ok(serde_json::from_value(value)?),
            }
        })
    }
}

/// The socket's client messages. (Karaka: `clientMessage`.)
#[derive(Debug, PartialEq, Eq)]
enum ClientMessage {
    Hello(String),
    Follow { id: String, chat: String },
    Stop(String),
    Pong,
}

fn client_message(text: &str) -> Option<ClientMessage> {
    let value: Value = serde_json::from_str(text).ok()?;
    let string = |map: &Map<String, Value>, key: &str| {
        map.get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let only = |keys: &[&str]| -> Option<&Map<String, Value>> {
        let map = value.as_object()?;
        map.keys()
            .all(|key| keys.contains(&key.as_str()))
            .then_some(map)
    };
    match value.get("type")?.as_str()? {
        "hello" => Some(ClientMessage::Hello(string(
            only(&["type", "credential"])?,
            "credential",
        )?)),
        "follow" => {
            let map = only(&["type", "id", "chatId"])?;
            Some(ClientMessage::Follow {
                id: string(map, "id")?,
                chat: string(map, "chatId")?,
            })
        }
        "stop" => Some(ClientMessage::Stop(string(only(&["type", "id"])?, "id")?)),
        "pong" => only(&["type"]).map(|_| ClientMessage::Pong),
        _ => None,
    }
}

/// What a socket's tasks hand its loop.
enum Out {
    Send(Value),
    FollowEnded(String),
}

impl Browser {
    /// The upgrade at `<path>/socket`: refused while starting or from
    /// another origin, then the WebSocket handshake. (Karaka:
    /// `registerUpgrade` and `ws`'s `handleUpgrade`.)
    pub async fn upgrade(self: Arc<Self>, mut request: Request, upgrades: Upgrades) -> Response {
        let refuse = |status: StatusCode| {
            let mut response = Response::new(body::empty());
            *response.status_mut() = status;
            response
                .headers_mut()
                .insert(CONNECTION, HeaderValue::from_static("close"));
            response
        };
        if !self.startup.is_ready() {
            return refuse(StatusCode::SERVICE_UNAVAILABLE);
        }
        let origin = request
            .headers()
            .get(ORIGIN)
            .and_then(|value| value.to_str().ok());
        if !origin.is_some_and(|origin| self.config.origins.iter().any(|known| known == origin)) {
            return refuse(StatusCode::FORBIDDEN);
        }
        let headers = request.headers();
        let header = |name: HeaderName| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let key = header(SEC_WEBSOCKET_KEY);
        let version = header(SEC_WEBSOCKET_VERSION);
        let websocket =
            header(UPGRADE).is_some_and(|upgrade| upgrade.eq_ignore_ascii_case("websocket"));
        let (Some(key), true, true) = (
            key.filter(|key| ws::valid_key(key.trim())),
            websocket && request.method() == Method::GET,
            matches!(version.as_deref().map(str::trim), Some("13" | "8")),
        ) else {
            // ws's `abortHandshake(socket, 400)`.
            let mut response = Response::new(body::full("Bad Request"));
            *response.status_mut() = StatusCode::BAD_REQUEST;
            let headers = response.headers_mut();
            headers.insert(CONNECTION, HeaderValue::from_static("close"));
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/html"));
            return response;
        };
        let accept = ws::accept_key(key.trim());
        let browser = Arc::clone(&self);
        upgrades.adopt(on_upgrade(&mut request), move |io| browser.serve(io));
        let mut response = Response::new(body::empty());
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        let headers = response.headers_mut();
        headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
        if let Ok(accept) = HeaderValue::from_str(&accept) {
            headers.insert(SEC_WEBSOCKET_ACCEPT, accept);
        }
        response
    }

    /// One socket: authenticate with the first message within two
    /// heartbeats, then interactions and follows until either side closes,
    /// the credential expires, or the client misses a heartbeat. (Karaka:
    /// `serve`.)
    async fn serve(self: Arc<Self>, io: TokioIo<Upgraded>) {
        let (read, mut write) = tokio::io::split(io);
        let (incoming_tx, mut incoming) = mpsc::unbounded_channel();
        let max = self.max_body_bytes;
        let reading = tokio::spawn(read_frames(read, max, incoming_tx));
        let (out_tx, mut out) = mpsc::unbounded_channel::<Out>();
        let (push_tx, mut pushed) = mpsc::unbounded_channel::<Value>();
        let heartbeat = Duration::from_millis(self.config.heartbeat_ms);
        let hello = tokio::time::sleep(2 * heartbeat);
        tokio::pin!(hello);
        let expiry = tokio::time::sleep(Duration::MAX / 4);
        tokio::pin!(expiry);
        let mut ticks =
            tokio::time::interval_at(tokio::time::Instant::now() + heartbeat, heartbeat);
        let mut heard = true;
        let mut caller: Option<BrowserCaller> = None;
        let mut channel: Option<String> = None;
        let mut follows: HashMap<String, oneshot::Sender<()>> = HashMap::new();
        type Admission = std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Option<BrowserCaller>, AppError>> + Send>,
        >;
        let mut admitting: Option<Admission> = None;
        let close = |code: u16, reason: &'static str| (Some(code), reason.to_owned());
        let ending: Option<(Option<u16>, String)> = loop {
            tokio::select! {
                // Timers first: Node fires the hello timer, made first, before
                // a heartbeat due at the same moment.
                biased;
                () = &mut hello, if caller.is_none() => break Some(close(CLOSE_UNAUTHORIZED, "Authentication timed out")),
                () = &mut expiry, if caller.is_some() => break Some(close(CLOSE_UNAUTHORIZED, "Browser credential expired")),
                message = incoming.recv() => {
                    let message = match message {
                        None | Some(Err(ReadError::Gone)) => break None,
                        Some(Err(ReadError::Close(code))) => break Some((Some(code), String::new())),
                        Some(Ok(message)) => message,
                    };
                    let text = match message {
                        Incoming::Ping(payload) => {
                            if ws::send(&mut write, ws::OP_PONG, &payload).await.is_err() {
                                break None;
                            }
                            continue;
                        }
                        Incoming::Pong => continue,
                        Incoming::Close(code, reason) => {
                            // ws echoes the peer's close.
                            let _ = ws::send(&mut write, ws::OP_CLOSE, &ws::close_payload(code, &reason)).await;
                            break None;
                        }
                        Incoming::Binary(_) => break Some(close(CLOSE_PROTOCOL, "Invalid message")),
                        Incoming::Text(text) => {
                            heard = true;
                            text
                        }
                    };
                    let Some(message) = client_message(&text) else {
                        break Some(close(CLOSE_PROTOCOL, "Invalid message"));
                    };
                    let Some(verified) = &caller else {
                        let ClientMessage::Hello(credential) = message else {
                            break Some(close(CLOSE_PROTOCOL, "Authenticate first"));
                        };
                        if admitting.is_some() {
                            break Some(close(CLOSE_PROTOCOL, "Authenticate first"));
                        }
                        let browser = Arc::clone(&self);
                        admitting = Some(Box::pin(async move {
                            let Some(verified) = browser.auth.authenticate(&credential) else {
                                return Ok(None);
                            };
                            browser.admit(&verified).await?;
                            Ok(Some(verified))
                        }));
                        continue;
                    };
                    match message {
                        ClientMessage::Hello(_) => break Some(close(CLOSE_PROTOCOL, "Already authenticated")),
                        ClientMessage::Pong => {}
                        ClientMessage::Stop(id) => {
                            if let Some(stop) = follows.remove(&id) {
                                let _ = stop.send(());
                            }
                        }
                        ClientMessage::Follow { id, chat } => {
                            if follows.contains_key(&id) {
                                break Some(close(CLOSE_PROTOCOL, "Duplicate follow id"));
                            }
                            let (stop, stopped) = oneshot::channel();
                            follows.insert(id.clone(), stop);
                            tokio::spawn(Arc::clone(&self).follow_over_socket(
                                verified.clone(),
                                id,
                                SessionId::new(chat),
                                stopped,
                                out_tx.clone(),
                            ));
                        }
                    }
                }
                admitted = async { admitting.as_mut().expect("polled only while admitting").await }, if admitting.is_some() => {
                    admitting = None;
                    match admitted {
                        Ok(None) => break Some(close(CLOSE_UNAUTHORIZED, "Invalid browser credential")),
                        Err(error) => {
                            let (_, envelope) = failure(&error);
                            let code = if envelope["error"]["code"] == "gateway/forbidden" { CLOSE_FORBIDDEN } else { CLOSE_FAILED };
                            break Some((Some(code), envelope["error"]["message"].as_str().unwrap_or_default().to_owned()));
                        }
                        Ok(Some(verified)) => {
                            let left = Duration::from_millis(verified.expires_at.saturating_sub(now_ms()));
                            expiry.as_mut().reset(tokio::time::Instant::now() + left);
                            let mut extra = Map::new();
                            extra.insert("heartbeatMs".into(), json!(self.config.heartbeat_ms));
                            channel = Some(self.join(&verified, push_tx.clone(), extra));
                            caller = Some(verified);
                        }
                    }
                }
                _ = ticks.tick() => {
                    if !heard {
                        // ws's `terminate`: the connection is cut, unclosed.
                        break None;
                    }
                    heard = false;
                    if ws::send(&mut write, ws::OP_TEXT, json!({ "type": "ping" }).to_string().as_bytes()).await.is_err() {
                        break None;
                    }
                }
                Some(event) = pushed.recv() => {
                    if ws::send(&mut write, ws::OP_TEXT, event.to_string().as_bytes()).await.is_err() {
                        break None;
                    }
                }
                Some(item) = out.recv() => match item {
                    Out::Send(message) => {
                        if ws::send(&mut write, ws::OP_TEXT, message.to_string().as_bytes()).await.is_err() {
                            break None;
                        }
                    }
                    Out::FollowEnded(id) => {
                        follows.remove(&id);
                    }
                },
            }
        };
        if let Some(id) = &channel {
            self.leave(id);
        }
        drop(follows);
        if let Some((code, reason)) = ending
            && ws::send(&mut write, ws::OP_CLOSE, &ws::close_payload(code, &reason))
                .await
                .is_ok()
        {
            {
                // Wait for the peer's close, as ws does, up to its 30 s.
                let _ = tokio::time::timeout(Duration::from_secs(30), async {
                    while let Some(message) = incoming.recv().await {
                        if matches!(message, Ok(Incoming::Close(..)) | Err(_)) {
                            break;
                        }
                    }
                })
                .await;
            }
        }
        reading.abort();
    }

    /// One follow on a socket: `frame` messages, the snapshot first, then
    /// `end`, which a `stop` also sends. (Karaka: `follow`.)
    async fn follow_over_socket(
        self: Arc<Self>,
        caller: BrowserCaller,
        id: String,
        chat: SessionId,
        stopped: oneshot::Receiver<()>,
        out: mpsc::UnboundedSender<Out>,
    ) {
        let frame =
            |result: Value| Out::Send(json!({ "type": "frame", "id": id, "result": result }));
        let run = async {
            if !self.allows_method("applicationFollow") {
                let error = AppError::coded("CHAT_FORBIDDEN", "Browser method is unavailable");
                let _ = out.send(frame(failure(&error).1));
                return;
            }
            let mut follow = match self.chats.follow(&Self::address(&caller, chat)).await {
                Ok(follow) => follow,
                Err(error) => {
                    let _ = out.send(frame(failure(&error).1));
                    return;
                }
            };
            let _ = out.send(frame(
                json!({ "ok": true, "value": snapshot_json(&follow) }),
            ));
            loop {
                match follow.next().await {
                    Ok(Some(next)) => {
                        let _ = out.send(frame(json!({ "ok": true, "value": frame_json(&next) })));
                    }
                    Ok(None) => return,
                    Err(error) => {
                        let _ = out.send(frame(failure(&error).1));
                        return;
                    }
                }
            }
        };
        tokio::select! {
            () = run => {}
            _ = stopped => {}
        }
        let _ = out.send(Out::Send(json!({ "type": "end", "id": id })));
        let _ = out.send(Out::FollowEnded(id));
    }
}

/// Reads frames until the connection ends, handing each to the socket's
/// loop: reading isn't cancelled mid-frame.
async fn read_frames(
    read: ReadHalf<TokioIo<Upgraded>>,
    max: usize,
    sender: mpsc::UnboundedSender<Result<Incoming, ReadError>>,
) {
    let mut reader = ws::Reader::new(read, true, max);
    loop {
        let message = reader.read().await;
        let stop = message.is_err();
        if sender.send(message).is_err() || stop {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientMessage, client_message};

    #[test]
    fn client_messages_are_strict() {
        assert_eq!(
            client_message(r#"{"type":"hello","credential":"x"}"#),
            Some(ClientMessage::Hello("x".into()))
        );
        assert_eq!(
            client_message(r#"{"type":"pong"}"#),
            Some(ClientMessage::Pong)
        );
        for bad in [
            "not json",
            "{}",
            r#"{"type":"subscribe"}"#,
            r#"{"type":"hello","credential":""}"#,
            r#"{"type":"pong","extra":1}"#,
            r#"{"type":"follow","id":"1"}"#,
        ] {
            assert_eq!(client_message(bad), None, "{bad}");
        }
    }
}
