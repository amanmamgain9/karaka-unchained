//! Anthropic and an OpenAI-compatible provider started in one runtime, as
//! Play This Life runs Anthropic beside DeepSeek. Each answers on its own
//! route, from its own fake server on localhost.
//!
//! It lives here because this is the one crate that depends on both
//! providers. The server helpers are copied from `cli.rs`: each file under
//! `tests/` is its own crate.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread::{self, JoinHandle};

use karaka_anthropic::{AnthropicConfig, AnthropicPlugin, CacheMode, Profile};
use karaka_kernel::Runtime;
use karaka_model::{Message, ModelRegistry, ModelsPlugin, Request, complete};
use karaka_openai::{OpenAiConfig, OpenAiPlugin};

/// Serves one request with `response`, written out exactly as given.
/// The handle gives back the request the server received.
fn serve_once(response: String) -> (SocketAddr, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let request = read_request(&mut socket);
        socket.write_all(response.as_bytes()).expect("write");
        request
    });
    (address, server)
}

/// Reads up to the end of the headers, then as many bytes of body as
/// `content-length` says.
fn read_request(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let mut total = None;
    loop {
        if total.is_none()
            && let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let head = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |value| value.trim().parse().expect("a number"));
            total = Some(end + 4 + length);
        }
        if total.is_some_and(|total| bytes.len() >= total) {
            return String::from_utf8(bytes).expect("the request is UTF-8");
        }
        let read = socket.read(&mut chunk).expect("read");
        assert!(read > 0, "the client hung up mid-request");
        bytes.extend_from_slice(&chunk[..read]);
    }
}

/// A streamed 200 response that closes the connection after `body`.
fn streamed(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Anthropic's events for the answer "from anthropic".
const ANTHROPIC_ANSWER: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":12,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"from anthropic"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

/// An OpenAI-compatible server's chunks for the answer "from deepseek".
const DEEPSEEK_ANSWER: &str = concat!(
    r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"from deepseek"},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

async fn ask(registry: &ModelRegistry, route: &str, model: &str) -> String {
    let found = registry.get(route).expect("the route is registered");
    let request = Request::new(model, vec![Message::user("hi")]);
    complete(found.as_ref(), request)
        .await
        .expect("the call works")
        .message
        .text()
}

#[tokio::test]
async fn anthropic_and_deepseek_answer_side_by_side() {
    let (anthropic_address, anthropic_server) = serve_once(streamed(ANTHROPIC_ANSWER));
    let (deepseek_address, deepseek_server) = serve_once(streamed(DEEPSEEK_ANSWER));

    let anthropic = AnthropicPlugin::new(AnthropicConfig {
        route: "anthropic".into(),
        api_key: "anthropic-key".into(),
        base_url: Some(format!("http://{anthropic_address}")),
        default_max_tokens: Some(100),
        default_effort: None,
        thinking_display: None,
        cache: CacheMode::Off,
        profile: Profile::Anthropic,
        thinking: None,
    });
    let deepseek = OpenAiPlugin::new(OpenAiConfig {
        route: "deepseek".into(),
        base_url: format!("http://{deepseek_address}"),
        api_key: Some("deepseek-key".into()),
        default_effort: None,
        thinking: None,
        max_tokens_field: Default::default(),
        system_messages_in_history: false,
    });

    let runtime = Runtime::start(vec![
        Box::new(ModelsPlugin),
        Box::new(anthropic),
        Box::new(deepseek),
    ])
    .await
    .expect("the runtime starts");
    assert_eq!(runtime.plugins(), ["models", "anthropic", "deepseek"]);

    let registry = runtime.get::<ModelRegistry>();
    assert_eq!(registry.routes(), ["anthropic", "deepseek"]);
    assert_eq!(
        ask(&registry, "anthropic", "claude-opus-5-5").await,
        "from anthropic"
    );
    assert_eq!(
        ask(&registry, "deepseek", "deepseek-chat").await,
        "from deepseek"
    );

    // Each server got its own provider's request, with its own key.
    let sent = anthropic_server
        .join()
        .expect("the server ran")
        .to_lowercase();
    assert!(sent.starts_with("post /v1/messages "), "{sent}");
    assert!(sent.contains("\r\nx-api-key: anthropic-key\r\n"), "{sent}");
    let sent = deepseek_server
        .join()
        .expect("the server ran")
        .to_lowercase();
    assert!(sent.starts_with("post /chat/completions "), "{sent}");
    assert!(
        sent.contains("\r\nauthorization: bearer deepseek-key\r\n"),
        "{sent}"
    );
}

/// DeepSeek's events for the answer "from deepseek": the same format as
/// Anthropic's.
const DEEPSEEK_MESSAGES_ANSWER: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_1","model":"deepseek-flash","usage":{"input_tokens":12,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"from deepseek"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

/// The way dsh v4 runs the two: both through the Messages format, each
/// with its own profile, route and key.
#[tokio::test]
async fn both_profiles_of_the_messages_crate_answer_side_by_side() {
    let (anthropic_address, anthropic_server) = serve_once(streamed(ANTHROPIC_ANSWER));
    let (deepseek_address, deepseek_server) = serve_once(streamed(DEEPSEEK_MESSAGES_ANSWER));

    let config = |route: &str, key: &str, base_url: String, profile| AnthropicConfig {
        route: route.into(),
        api_key: key.into(),
        base_url: Some(base_url),
        default_max_tokens: None,
        default_effort: None,
        thinking_display: None,
        cache: CacheMode::Off,
        profile,
        thinking: None,
    };
    let anthropic = config(
        "anthropic",
        "anthropic-key",
        format!("http://{anthropic_address}"),
        Profile::Anthropic,
    );
    let deepseek = config(
        "deepseek",
        "deepseek-key",
        format!("http://{deepseek_address}/anthropic"),
        Profile::DeepSeek,
    );

    let runtime = Runtime::start(vec![
        Box::new(ModelsPlugin),
        Box::new(AnthropicPlugin::new(anthropic)),
        Box::new(AnthropicPlugin::new(deepseek)),
    ])
    .await
    .expect("the runtime starts");
    let registry = runtime.get::<ModelRegistry>();
    assert_eq!(registry.routes(), ["anthropic", "deepseek"]);
    assert_eq!(
        ask(&registry, "anthropic", "claude-opus-5-5").await,
        "from anthropic"
    );
    assert_eq!(
        ask(&registry, "deepseek", "deepseek-flash").await,
        "from deepseek"
    );

    let sent = anthropic_server
        .join()
        .expect("the server ran")
        .to_lowercase();
    assert!(sent.starts_with("post /v1/messages "), "{sent}");
    assert!(
        sent.contains("\r\nanthropic-version: 2026-10-21\r\n"),
        "{sent}"
    );
    let sent = deepseek_server
        .join()
        .expect("the server ran")
        .to_lowercase();
    assert!(sent.starts_with("post /anthropic/v1/messages "), "{sent}");
    assert!(sent.contains("\r\nx-api-key: deepseek-key\r\n"), "{sent}");
    assert!(
        sent.contains("\r\nanthropic-version: 2023-06-01\r\n"),
        "{sent}"
    );
    assert!(sent.contains(r#""max_tokens":256000"#), "{sent}");
}
