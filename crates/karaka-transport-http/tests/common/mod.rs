//! A whole runtime behind the routes: a scripted model, two presets, JSONL
//! sessions in a temporary directory, the identity, a fixed server
//! credential, and the webserver on a loopback port. (Karaka:
//! `application-fixture.ts` and `routes-fixture.ts`, which stub the
//! services instead.)

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_core::Stream;
use karaka_agent::{Agent, ModelSelection};
use karaka_agent_default_model::AgentDefaultModelPlugin;
use karaka_agent_loop::{AgentLoopPlugin, LoopConfig};
use karaka_agent_presets::{AgentPresetsConfig, AgentPresetsPlugin, PresetDefinition};
use karaka_browser_auth::{
    Algorithm, BrowserAuthConfig, BrowserAuthPlugin, KeyConfig, encode_standard, encode_url,
};
use karaka_identity::{ApplicationId, KarakaIdentityPlugin};
use karaka_kernel::Runtime;
use karaka_model::{
    Block, ErrorKind, Event, EventStream, FinishReason, Model, ModelError, ModelRegistry,
    ModelsPlugin, Request, Usage, stream_of,
};
use karaka_server_auth::{AuthFuture, AuthenticatedApplication, ServerAuth, ServerAuthPlugin};
use karaka_session::{SessionId, SessionsPlugin};
use karaka_session_jsonl::{JsonlConfig, JsonlPlugin};
use karaka_session_persistence::DurableSessionsPlugin;
use karaka_session_projection::SessionProjectionsPlugin;
use karaka_session_query::SessionQueryPlugin;
use karaka_startup::{KarakaStartup, KarakaStartupPlugin};
use karaka_system_prompt::{SystemPromptConfig, SystemPromptPlugin};
use karaka_tools::ToolsPlugin;
use karaka_transport_http::{BrowserConfig, TransportConfig, TransportHttpPlugin};
use karaka_user_approval::{ApprovalConfig, ApprovalPlugin};
use karaka_user_questions::UserQuestionsPlugin;
use karaka_webserver::{WebServer, WebServerConfig, WebServerPlugin};
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// The credential the application server sends.
pub const CREDENTIAL: &str = "Bearer chat-secret";

/// Verifies [`CREDENTIAL`] as the application `app`.
struct Fixed;

impl ServerAuth for Fixed {
    fn authenticate<'a>(
        &'a self,
        authorization: Option<&'a str>,
    ) -> AuthFuture<'a, Option<AuthenticatedApplication>> {
        Box::pin(async move {
            Ok(
                (authorization == Some(CREDENTIAL)).then(|| AuthenticatedApplication {
                    application_id: ApplicationId::new("app"),
                }),
            )
        })
    }

    fn authorize_tools<'a>(&'a self, _: &'a ApplicationId) -> AuthFuture<'a, String> {
        Box::pin(async { Err("not used".into()) })
    }
}

/// Plays one scripted text reply per call, and keeps every request.
#[derive(Default)]
pub struct Scripted {
    pub replies: Mutex<VecDeque<String>>,
    pub requests: Mutex<Vec<Request>>,
}

impl Model for Scripted {
    fn provider(&self) -> &str {
        "mock"
    }

    fn stream(&self, request: Request) -> EventStream {
        self.requests.lock().unwrap().push(request);
        let events: Vec<Result<Event, ModelError>> = match self.replies.lock().unwrap().pop_front()
        {
            Some(text) => stream_of(&[Block::Text(text)], FinishReason::Stop, Usage::default())
                .into_iter()
                .map(Ok)
                .collect(),
            None => vec![Err(ModelError::new(ErrorKind::InvalidRequest, "exhausted"))],
        };
        Box::pin(Playback(events.into()))
    }
}

struct Playback(VecDeque<Result<Event, ModelError>>);

impl Stream for Playback {
    type Item = Result<Event, ModelError>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().0.pop_front())
    }
}

pub struct Server {
    pub runtime: Runtime,
    pub port: u16,
    pub model: Arc<Scripted>,
    pub root: PathBuf,
    pub signer: Signer,
}

/// A fresh directory of the test's own.
pub fn scratch() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "karaka-transport-http-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub struct Options {
    /// Browser access, verified with [`Server::signer`]'s key.
    pub browser: Option<BrowserConfig>,
    pub root: PathBuf,
    pub config: TransportConfig,
    pub default_provider: &'static str,
    pub ready: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            browser: None,
            root: scratch(),
            config: TransportConfig {
                path: "/api".into(),
                ..TransportConfig::default()
            },
            default_provider: "mock",
            ready: true,
        }
    }
}

/// The browser endpoint the tests mount.
pub fn browser() -> BrowserConfig {
    BrowserConfig::new(
        "/browser",
        vec!["https://app.example".into()],
        vec!["main".into()],
    )
}

/// The origin browsers send.
pub const ORIGIN: &str = "https://app.example";

/// Signs browser credentials with an ES256 key whose SPKI the runtime
/// trusts as `key`.
pub struct Signer {
    key: EcdsaKeyPair,
    pub public: String,
}

const P256_SPKI: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

impl Signer {
    pub fn new() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let mut der = P256_SPKI.to_vec();
        der.extend_from_slice(key.public_key().as_ref());
        let body = encode_standard(&der);
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|chunk| std::str::from_utf8(chunk).unwrap())
            .collect();
        Self {
            public: format!(
                "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
                lines.join("\n")
            ),
            key,
        }
    }

    /// A credential for `user` on `chat`, valid for `seconds`.
    pub fn token(&self, user: &str, chat: &str, seconds: u64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = json!({
            "applicationId": "app", "tenantId": "tenant", "userId": user, "chatId": chat,
            "iss": "issuer", "aud": "karaka", "iat": now, "exp": now + seconds,
        });
        let message = format!(
            "{}.{}",
            encode_url(
                json!({ "alg": "ES256", "kid": "key" })
                    .to_string()
                    .as_bytes()
            ),
            encode_url(claims.to_string().as_bytes())
        );
        let signature = self
            .key
            .sign(&SystemRandom::new(), message.as_bytes())
            .unwrap();
        format!("{message}.{}", encode_url(signature.as_ref()))
    }
}

pub async fn start(options: Options) -> Server {
    let signer = Signer::new();
    let presets = AgentPresetsPlugin::new(AgentPresetsConfig::new("main"))
        .preset(PresetDefinition {
            name: Some("Main".into()),
            ..PresetDefinition::new("main", Vec::new())
        })
        .preset(PresetDefinition::new("plain", Vec::new()));
    let runtime = Runtime::start(vec![
        Box::new(SessionsPlugin),
        Box::new(SessionProjectionsPlugin),
        Box::new(JsonlPlugin::new(JsonlConfig::new(options.root.clone()))),
        Box::new(DurableSessionsPlugin),
        Box::new(SystemPromptPlugin::new(SystemPromptConfig::default())),
        Box::new(ToolsPlugin),
        Box::new(ModelsPlugin),
        Box::new(AgentLoopPlugin::new(LoopConfig::new("mock", "chat"))),
        Box::new(UserQuestionsPlugin),
        Box::new(ApprovalPlugin::new(ApprovalConfig::default())),
        Box::new(BrowserAuthPlugin::new(BrowserAuthConfig {
            application_id: "app".into(),
            issuer: "issuer".into(),
            audience: "karaka".into(),
            max_token_age_seconds: 300,
            keys: vec![KeyConfig {
                id: "key".into(),
                algorithm: Algorithm::Es256,
                public_key: signer.public.clone(),
            }],
        })),
        Box::new(KarakaIdentityPlugin::with_persistence()),
        Box::new(ServerAuthPlugin::with(Arc::new(Fixed))),
        Box::new(presets),
        Box::new(WebServerPlugin::new(
            WebServerConfig::new("127.0.0.1", 0).unwrap(),
        )),
        Box::new(KarakaStartupPlugin),
        Box::new(SessionQueryPlugin::with_persistence()),
        Box::new(AgentDefaultModelPlugin::new(ModelSelection {
            provider: options.default_provider.into(),
            model: "chat".into(),
            reasoning_effort: None,
        })),
        Box::new(
            TransportHttpPlugin::new(
                TransportConfig {
                    browser: options.browser,
                    ..options.config
                },
                true,
            )
            .unwrap(),
        ),
    ])
    .await
    .unwrap();
    let model = Arc::new(Scripted::default());
    runtime
        .get::<ModelRegistry>()
        .register("mock", Arc::clone(&model) as Arc<dyn Model>)
        .unwrap();
    if options.ready {
        runtime.get::<KarakaStartup>().commit();
    }
    let port = runtime.get::<WebServer>().port();
    Server {
        runtime,
        port,
        model,
        root: options.root,
        signer,
    }
}

/// The tenant and user of the chats.
pub fn identity() -> Value {
    json!({ "tenantId": "tenant", "userId": "user" })
}

/// `identity()` with `extra`'s fields.
pub fn with(extra: Value) -> Value {
    let mut body = identity();
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    body
}

/// One response: status, head (lower-cased) and body.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub head: String,
    pub body: String,
}

impl Answer {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

fn dechunk(mut body: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.push_str(&rest[..size]);
        body = &rest[size + 2..];
    }
    out
}

impl Server {
    /// A request with `body` as raw text, and the credential unless
    /// `credential` is `None`.
    pub async fn raw(
        &self,
        method: &str,
        path: &str,
        body: &str,
        credential: Option<&str>,
    ) -> Answer {
        let mut socket = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let authorization =
            credential.map_or(String::new(), |value| format!("Authorization: {value}\r\n"));
        socket
            .write_all(
                format!(
                    "{method} {path} HTTP/1.1\r\nHost: x\r\n{authorization}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.unwrap();
        let raw = String::from_utf8_lossy(&raw).to_string();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        let head = head.to_ascii_lowercase();
        let body = if head.contains("transfer-encoding: chunked") {
            dechunk(body)
        } else {
            body.to_owned()
        };
        Answer { status, head, body }
    }

    /// A request with `headers` as given, and nothing else.
    pub async fn with_headers(
        &self,
        method: &str,
        path: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Answer {
        let mut socket = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let mut head = String::new();
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        socket
            .write_all(
                format!(
                    "{method} {path} HTTP/1.1\r\nHost: x\r\n{head}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.unwrap();
        let raw = String::from_utf8_lossy(&raw).to_string();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        let head = head.to_ascii_lowercase();
        let body = if head.contains("transfer-encoding: chunked") {
            dechunk(body)
        } else {
            body.to_owned()
        };
        Answer { status, head, body }
    }

    /// A browser method call with `token`.
    pub async fn browser(&self, method: &str, body: &Value, token: &str) -> Answer {
        let authorization = format!("Bearer {token}");
        self.with_headers(
            "POST",
            &format!("/browser/{method}"),
            &body.to_string(),
            &[
                ("Origin", ORIGIN),
                ("Authorization", &authorization),
                ("Content-Type", "application/json"),
            ],
        )
        .await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Answer {
        self.raw("POST", path, &body.to_string(), Some(CREDENTIAL))
            .await
    }

    pub async fn get(&self, path: &str) -> Answer {
        self.raw("GET", path, "", Some(CREDENTIAL)).await
    }

    /// Opens a stream with `headers`.
    pub async fn stream_with(&self, path: &str, body: &Value, headers: &[(&str, &str)]) -> Events {
        let mut socket = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let body = body.to_string();
        let mut head = String::new();
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        socket
            .write_all(
                format!(
                    "POST {path} HTTP/1.1\r\nHost: x\r\n{head}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut reader = BufReader::new(socket);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            head.push_str(&line.to_ascii_lowercase());
        }
        Events { reader, head }
    }

    /// Opens a stream: the response head, then its events one by one.
    pub async fn stream(&self, path: &str, body: &Value) -> Events {
        let mut socket = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let body = body.to_string();
        socket
            .write_all(
                format!(
                    "POST {path} HTTP/1.1\r\nHost: x\r\nAuthorization: {CREDENTIAL}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut reader = BufReader::new(socket);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            head.push_str(&line.to_ascii_lowercase());
        }
        Events { reader, head }
    }

    /// Waits until the chat's agent is idle.
    pub async fn idle(&self, chat: &str) {
        let agent = self
            .runtime
            .get::<dyn karaka_agent::Agents>()
            .get(&SessionId::new(chat))
            .unwrap();
        agent.when_idle().await;
    }

    pub fn agent(&self, chat: &str) -> Arc<dyn Agent> {
        self.runtime
            .get::<dyn karaka_agent::Agents>()
            .get(&SessionId::new(chat))
            .unwrap()
    }
}

/// An SSE response's events, read from a chunked body.
pub struct Events {
    reader: BufReader<TcpStream>,
    pub head: String,
}

impl Events {
    /// The next event, or `None` at the end.
    pub async fn next(&mut self) -> Option<Value> {
        let mut data = String::new();
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.reader.read_line(&mut line),
            )
            .await
            .expect("an event within 10 s")
            .ok()?;
            if read == 0 {
                return None;
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(rest) = line.strip_prefix("data: ") {
                data.push_str(rest);
                continue;
            }
            if line.is_empty() && !data.is_empty() {
                return Some(serde_json::from_str(&data).unwrap());
            }
            if line == "0" && data.is_empty() {
                // The chunked body's end.
                return None;
            }
        }
    }

    /// Events up to and including the first of `kind`.
    pub async fn until(&mut self, kind: &str) -> Vec<Value> {
        let mut seen = Vec::new();
        while let Some(event) = self.next().await {
            let done = event["type"] == kind;
            seen.push(event);
            if done {
                break;
            }
        }
        seen
    }
}

/// A WebSocket client of the browser socket, written against the codec.
pub struct Socket {
    reader: karaka_transport_http::ws::Reader<tokio::io::ReadHalf<TcpStream>>,
    writer: tokio::io::WriteHalf<TcpStream>,
}

/// What the upgrade answered: a socket, or the refusal's status.
pub enum Opened {
    Socket(Socket),
    Refused(u16),
}

impl Server {
    /// Opens the browser socket from `origin`.
    pub async fn socket(&self, origin: Option<&str>) -> Opened {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let origin = origin.map_or(String::new(), |origin| format!("Origin: {origin}\r\n"));
        stream
            .write_all(
                format!(
                    "GET /browser/socket HTTP/1.1\r\nHost: x\r\n{origin}Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).to_string();
        let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
        if status != 101 {
            return Opened::Refused(status);
        }
        assert!(
            head.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            "the accept key for the RFC's sample: {head}"
        );
        let (read, writer) = tokio::io::split(stream);
        Opened::Socket(Socket {
            reader: karaka_transport_http::ws::Reader::new(read, false, 1 << 24),
            writer,
        })
    }

    pub async fn open(&self) -> Socket {
        match self.socket(Some(ORIGIN)).await {
            Opened::Socket(socket) => socket,
            Opened::Refused(status) => panic!("refused: {status}"),
        }
    }
}

/// How a socket ended, as the client saw it.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// A close frame, its code and reason.
    Closed(Option<u16>, String),
    /// The connection cut without one: ws's 1006.
    Cut,
}

impl Socket {
    pub async fn frame(&mut self, opcode: u8, payload: &[u8]) {
        let frame = karaka_transport_http::ws::encode(opcode, payload, Some([1, 2, 3, 4]));
        self.writer.write_all(&frame).await.unwrap();
    }

    pub async fn send(&mut self, message: &Value) {
        self.frame(
            karaka_transport_http::ws::OP_TEXT,
            message.to_string().as_bytes(),
        )
        .await;
    }

    /// The next message, or how the socket ended; pongs to the server's
    /// WebSocket pings are left to the reader.
    pub async fn read(&mut self) -> Result<Value, Ended> {
        loop {
            let read = tokio::time::timeout(std::time::Duration::from_secs(10), self.reader.read())
                .await
                .expect("a message within 10 s");
            match read {
                Ok(karaka_transport_http::ws::Incoming::Text(text)) => {
                    return Ok(serde_json::from_str(&text).unwrap());
                }
                Ok(karaka_transport_http::ws::Incoming::Close(code, reason)) => {
                    let payload = karaka_transport_http::ws::close_payload(code, &reason);
                    let frame = karaka_transport_http::ws::encode(
                        karaka_transport_http::ws::OP_CLOSE,
                        &payload,
                        Some([0; 4]),
                    );
                    let _ = self.writer.write_all(&frame).await;
                    return Err(Ended::Closed(code, reason));
                }
                Ok(_) => continue,
                Err(_) => return Err(Ended::Cut),
            }
        }
    }

    /// Reads until the socket ends.
    pub async fn ended(&mut self) -> Ended {
        loop {
            if let Err(ended) = self.read().await {
                return ended;
            }
        }
    }

    /// Says hello with `token`, and reads `ready`.
    pub async fn hello(&mut self, token: &str) -> Value {
        self.send(&json!({ "type": "hello", "credential": token }))
            .await;
        let ready = self.read().await.unwrap();
        assert_eq!(ready["type"], "ready", "{ready}");
        ready
    }
}
