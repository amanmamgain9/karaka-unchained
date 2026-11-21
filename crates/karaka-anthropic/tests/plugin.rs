//! `AnthropicPlugin` started in a Karaka runtime, and the model it registers
//! called against a fake server on localhost.
//!
//! The server helpers are copied from `http.rs`: each file under `tests/` is
//! its own crate, and sharing code between them needs a module that both
//! include, which isn't worth it for three small functions.

use std::net::SocketAddr;
use std::sync::Arc;

use karaka_anthropic::{
    AnthropicConfig, AnthropicPlugin, CacheMode, DeepSeekThinking, Profile, ThinkingDisplay,
};
use karaka_kernel::{DynPlugin, Runtime, StartError};
use karaka_model::{Effort, Message, Model, ModelRegistry, ModelsPlugin, Request, complete};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Serves one request with `response`, written out exactly as given.
/// The handle gives back the request the server received.
async fn serve_once(response: String) -> (SocketAddr, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let request = read_request(&mut socket).await;
        socket.write_all(response.as_bytes()).await.expect("write");
        socket.shutdown().await.expect("shutdown");
        request
    });
    (address, server)
}

/// Reads up to the end of the headers, then as many bytes of body as
/// `content-length` says.
async fn read_request(socket: &mut TcpStream) -> String {
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
        let read = socket.read(&mut chunk).await.expect("read");
        assert!(read > 0, "the client hung up mid-request");
        bytes.extend_from_slice(&chunk[..read]);
    }
}

/// A streamed 200 response holding `events`, framed as Anthropic frames
/// them: the type as the `event` field, the JSON as `data`.
fn streamed(events: &[&str]) -> String {
    let body: String = events
        .iter()
        .map(|data| {
            let event: Value = serde_json::from_str(data).expect("valid JSON");
            format!(
                "event: {}\ndata: {data}\n\n",
                event["type"].as_str().expect("a type")
            )
        })
        .collect();
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

const HELLO: &[&str] = &[
    r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":20,"output_tokens":1}}}"#,
    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
    r#"{"type":"content_block_stop","index":0}"#,
    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}"#,
    r#"{"type":"message_stop"}"#,
];

fn plugin(route: &str, api_key: &str, address: SocketAddr) -> AnthropicPlugin {
    AnthropicPlugin::new(AnthropicConfig {
        route: route.into(),
        api_key: api_key.into(),
        base_url: Some(format!("http://{address}")),
        default_max_tokens: Some(100),
        default_effort: Some(Effort::Low),
        thinking_display: Some(ThinkingDisplay::Summarized),
        cache: CacheMode::Off,
        profile: Profile::Anthropic,
        thinking: None,
    })
}

/// The registry's plugin, then `provider`.
fn with_registry(provider: AnthropicPlugin) -> Vec<Box<dyn DynPlugin>> {
    vec![Box::new(ModelsPlugin), Box::new(provider)]
}

#[tokio::test]
async fn the_plugin_registers_a_model_that_answers() {
    let (address, server) = serve_once(streamed(HELLO)).await;

    let runtime = Runtime::start(with_registry(plugin("anthropic", "test-key", address)))
        .await
        .expect("the runtime starts");
    assert_eq!(runtime.plugins(), ["models", "anthropic"]);
    let registry = runtime.get::<ModelRegistry>();
    assert_eq!(registry.routes(), ["anthropic"]);
    let model: Arc<dyn Model> = registry.get("anthropic").expect("the route is registered");

    let request = Request::new("claude-opus-5-5", vec![Message::user("hi")]);
    let response = complete(model.as_ref(), request)
        .await
        .expect("the call works");
    assert_eq!(response.message.text(), "Hello");

    // The config reached the model: its key, its defaults for a request
    // that sets neither `max_tokens` nor effort, its thinking display, and
    // its cache mode.
    let sent = server.await.expect("the server ran");
    let (head, body) = sent.split_once("\r\n\r\n").expect("headers, then a body");
    assert!(
        head.to_lowercase().contains("\r\nx-api-key: test-key\r\n"),
        "{head}"
    );
    let body: Value = serde_json::from_str(body).expect("the body is JSON");
    assert_eq!(body["max_tokens"], 100);
    assert_eq!(body["output_config"]["effort"], "low");
    assert_eq!(body["thinking"]["display"], "summarized");
    assert!(!body.to_string().contains("cache_control"), "{body}");
}

#[tokio::test]
async fn an_empty_api_key_stops_the_runtime_from_starting() {
    // Nothing listens on port 1, and nothing should try to.
    let address = SocketAddr::from(([127, 0, 0, 1], 1));

    let Err(StartError::Failed {
        plugin: name,
        source,
    }) = Runtime::start(with_registry(plugin("anthropic", "", address))).await
    else {
        panic!("expected the plugin to fail");
    };
    assert_eq!(name, "anthropic");
    assert_eq!(source.to_string(), "the Anthropic API key is empty");
}

#[tokio::test]
async fn the_plugin_is_named_after_its_route() {
    let address = SocketAddr::from(([127, 0, 0, 1], 1));

    let runtime = Runtime::start(with_registry(plugin("anthropic-team", "k", address)))
        .await
        .expect("the runtime starts");
    assert_eq!(runtime.plugins(), ["models", "anthropic-team"]);
    assert_eq!(runtime.get::<ModelRegistry>().routes(), ["anthropic-team"]);
}

#[tokio::test]
async fn an_empty_route_stops_the_runtime_from_starting() {
    let address = SocketAddr::from(([127, 0, 0, 1], 1));

    let Err(StartError::Failed { source, .. }) =
        Runtime::start(with_registry(plugin("", "k", address))).await
    else {
        panic!("expected the plugin to fail");
    };
    assert!(
        source.to_string().starts_with("the model route is empty"),
        "{source}"
    );
}

// The DeepSeek profile.

/// Play This Life's DeepSeek setup (`karaka.cordis.yml`, `llm-deepseek`):
/// thinking on, effort max, `max_tokens` 393,216.
fn deepseek_config(address: SocketAddr) -> AnthropicConfig {
    AnthropicConfig {
        route: "deepseek".into(),
        api_key: "deepseek-key".into(),
        base_url: Some(format!("http://{address}/anthropic")),
        default_max_tokens: Some(393_216),
        default_effort: Some(Effort::Max),
        thinking_display: None,
        cache: CacheMode::default(),
        profile: Profile::DeepSeek,
        thinking: Some(DeepSeekThinking::Enabled),
    }
}

#[tokio::test]
async fn the_deepseek_profile_registers_a_model_that_answers() {
    let (address, server) = serve_once(streamed(HELLO)).await;

    let runtime = Runtime::start(with_registry(AnthropicPlugin::new(deepseek_config(
        address,
    ))))
    .await
    .expect("the runtime starts");
    assert_eq!(runtime.plugins(), ["models", "deepseek"]);
    let model = runtime
        .get::<ModelRegistry>()
        .get("deepseek")
        .expect("the route is registered");

    let request = Request::new("deepseek-flash", vec![Message::user("hi")]);
    let response = complete(model.as_ref(), request)
        .await
        .expect("the call works");
    assert_eq!(response.message.text(), "Hello");

    let sent = server.await.expect("the server ran");
    let (head, body) = sent.split_once("\r\n\r\n").expect("headers, then a body");
    let head = head.to_lowercase();
    assert!(head.starts_with("post /anthropic/v1/messages "), "{head}");
    assert!(head.contains("\r\nx-api-key: deepseek-key\r\n"), "{head}");
    assert!(
        head.contains("\r\nanthropic-version: 2023-06-01\r\n"),
        "{head}"
    );
    let body: Value = serde_json::from_str(body).expect("the body is JSON");
    assert_eq!(
        body,
        serde_json::json!({
            "model": "deepseek-flash",
            "stream": true,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "max_tokens": 393216,
            "thinking": {"type": "enabled"},
            "output_config": {"effort": "max"}
        })
    );
}

/// The error the DeepSeek plugin fails to start with, given `config`.
async fn deepseek_start_error(config: AnthropicConfig) -> String {
    let Err(StartError::Failed { source, .. }) =
        Runtime::start(with_registry(AnthropicPlugin::new(config))).await
    else {
        panic!("expected the plugin to fail");
    };
    source.to_string()
}

#[tokio::test]
async fn a_deepseek_config_that_cant_work_stops_the_runtime() {
    // Nothing listens on port 1, and nothing should try to.
    let address = SocketAddr::from(([127, 0, 0, 1], 1));
    let cases = [
        (
            AnthropicConfig {
                api_key: " ".into(),
                ..deepseek_config(address)
            },
            "the DeepSeek API key is empty",
        ),
        (
            AnthropicConfig {
                default_effort: Some(Effort::Medium),
                ..deepseek_config(address)
            },
            "the default effort is \"medium\", but DeepSeek takes only low, high or max",
        ),
        (
            AnthropicConfig {
                thinking: Some(DeepSeekThinking::Disabled),
                ..deepseek_config(address)
            },
            "the default effort is \"max\", but thinking is disabled, and DeepSeek takes no effort then",
        ),
        (
            AnthropicConfig {
                thinking_display: Some(ThinkingDisplay::Summarized),
                ..deepseek_config(address)
            },
            "`thinking_display` is for the anthropic profile; the deepseek profile turns thinking on or off with `thinking`",
        ),
        (
            AnthropicConfig {
                base_url: Some("https://user:secret@api.deepseek.com/anthropic".into()),
                ..deepseek_config(address)
            },
            "the base URL \"https://user:secret@api.deepseek.com/anthropic\" must be plain HTTP or HTTPS, with no user name, password, query or fragment",
        ),
    ];
    for (config, expected) in cases {
        assert_eq!(deepseek_start_error(config).await, expected);
    }
}

#[tokio::test]
async fn deepseeks_thinking_switch_is_refused_for_anthropic() {
    let address = SocketAddr::from(([127, 0, 0, 1], 1));
    let config = AnthropicConfig {
        route: "anthropic".into(),
        profile: Profile::Anthropic,
        ..deepseek_config(address)
    };
    assert_eq!(
        deepseek_start_error(config).await,
        "`thinking` is for the deepseek profile; the anthropic profile uses `thinking_display`"
    );
}
