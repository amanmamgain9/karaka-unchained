//! `OpenAiPlugin` started in a Karaka runtime, and the model it registers
//! called against a fake server on localhost. Several of them can share one
//! runtime, each under its own route.
//!
//! The server helpers are copied from `http.rs`: each file under `tests/` is
//! its own crate, and sharing code between them needs a module that both
//! include, which isn't worth it for three small functions.

use std::net::SocketAddr;
use std::sync::Arc;

use karaka_kernel::{BoxError, DynPlugin, Plugin, PluginContext, PortId, Runtime, StartError};
use karaka_model::{
    Effort, Message, Model, ModelRegistry, ModelsPlugin, Request, RetryPolicy, RetryPolicyConfig,
    complete,
};
use karaka_openai::{MaxTokensField, OpenAiConfig, OpenAiModel, OpenAiPlugin};
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

/// A streamed 200 response holding `chunks`, framed as OpenAI frames them:
/// only `data`, with a blank line after each.
fn streamed(chunks: &[&str]) -> String {
    let body: String = chunks
        .iter()
        .map(|data| format!("data: {data}\n\n"))
        .collect();
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

const HELLO: &[&str] = &[
    r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#,
    r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    r#"{"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":5}}"#,
    "[DONE]",
];

/// A streamed answer of just `text`.
fn answer_of(text: &str) -> String {
    let first = format!(
        r#"{{"choices":[{{"index":0,"delta":{{"role":"assistant","content":"{text}"}},"finish_reason":null}}]}}"#
    );
    streamed(&[
        &first,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ])
}

/// A config for `route` at `base_url`, with nothing else set.
fn config(route: &str, base_url: String) -> OpenAiConfig {
    OpenAiConfig {
        route: route.into(),
        base_url,
        api_key: None,
        default_effort: None,
        thinking: None,
        max_tokens_field: MaxTokensField::MaxTokens,
        system_messages_in_history: false,
        retry_policy: None,
    }
}

/// The registry's plugin, then `providers`.
fn with_registry(providers: Vec<OpenAiPlugin>) -> Vec<Box<dyn DynPlugin>> {
    let mut plugins: Vec<Box<dyn DynPlugin>> = vec![Box::new(ModelsPlugin)];
    for provider in providers {
        plugins.push(Box::new(provider));
    }
    plugins
}

/// The text the model at `route` answers with.
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
async fn the_plugin_registers_a_model_that_answers() {
    let (address, server) = serve_once(streamed(HELLO)).await;
    let plugin = OpenAiPlugin::new(OpenAiConfig {
        route: "openai".into(),
        base_url: format!("http://{address}/v1"),
        api_key: Some("test-key".into()),
        default_effort: Some(Effort::High),
        thinking: Some(true),
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
        system_messages_in_history: false,
        retry_policy: None,
    });

    let runtime = Runtime::start(with_registry(vec![plugin]))
        .await
        .expect("the runtime starts");
    assert_eq!(runtime.plugins(), ["models", "openai"]);
    let model: Arc<dyn Model> = runtime
        .get::<ModelRegistry>()
        .get("openai")
        .expect("the route is registered");

    let mut request = Request::new("gpt-test", vec![Message::user("hi")]);
    request.max_tokens = Some(50);
    let response = complete(model.as_ref(), request)
        .await
        .expect("the call works");
    assert_eq!(response.message.text(), "Hello");

    // The config reached the model: its base URL, its key, its default
    // effort, its thinking switch and its choice of max-tokens field.
    let sent = server.await.expect("the server ran");
    let (head, body) = sent.split_once("\r\n\r\n").expect("headers, then a body");
    let head = head.to_lowercase();
    assert!(head.starts_with("post /v1/chat/completions "), "{head}");
    assert!(
        head.contains("\r\nauthorization: bearer test-key\r\n"),
        "{head}"
    );
    let body: Value = serde_json::from_str(body).expect("the body is JSON");
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["max_completion_tokens"], 50);
    assert!(body.get("max_tokens").is_none(), "{body}");
}

#[tokio::test]
async fn an_empty_base_url_stops_the_runtime_from_starting() {
    let plugin = OpenAiPlugin::new(config("openai", String::new()));

    let Err(StartError::Failed {
        plugin: name,
        source,
    }) = Runtime::start(with_registry(vec![plugin])).await
    else {
        panic!("expected the plugin to fail");
    };
    assert_eq!(name, "openai");
    assert_eq!(
        source.to_string(),
        "the OpenAI-compatible base URL is empty"
    );
}

#[tokio::test]
async fn a_default_effort_with_thinking_off_stops_the_runtime_from_starting() {
    let plugin = OpenAiPlugin::new(OpenAiConfig {
        route: "deepseek".into(),
        base_url: "http://127.0.0.1:1".into(),
        api_key: None,
        default_effort: Some(Effort::Low),
        thinking: Some(false),
        max_tokens_field: MaxTokensField::MaxTokens,
        system_messages_in_history: false,
        retry_policy: None,
    });

    let Err(StartError::Failed { source, .. }) = Runtime::start(with_registry(vec![plugin])).await
    else {
        panic!("expected the plugin to fail");
    };
    assert_eq!(
        source.to_string(),
        "the default effort is Low, but thinking is turned off"
    );
}

#[tokio::test]
async fn two_routes_each_answer_from_their_own_server() {
    let (deepseek_address, deepseek_server) = serve_once(answer_of("from deepseek")).await;
    let (ollama_address, ollama_server) = serve_once(answer_of("from ollama")).await;

    let runtime = Runtime::start(with_registry(vec![
        OpenAiPlugin::new(config("deepseek", format!("http://{deepseek_address}"))),
        OpenAiPlugin::new(config("ollama", format!("http://{ollama_address}/v1"))),
    ]))
    .await
    .expect("the runtime starts");
    assert_eq!(runtime.plugins(), ["models", "deepseek", "ollama"]);

    let registry = runtime.get::<ModelRegistry>();
    assert_eq!(registry.routes(), ["deepseek", "ollama"]);
    assert_eq!(
        ask(&registry, "deepseek", "deepseek-chat").await,
        "from deepseek"
    );
    assert_eq!(ask(&registry, "ollama", "llama3").await, "from ollama");

    // Each server got only its own request, with its own model id.
    let deepseek_sent = deepseek_server.await.expect("the server ran");
    assert!(
        deepseek_sent.starts_with("POST /chat/completions "),
        "{deepseek_sent}"
    );
    assert!(
        deepseek_sent.contains(r#""model":"deepseek-chat""#),
        "{deepseek_sent}"
    );
    let ollama_sent = ollama_server.await.expect("the server ran");
    assert!(
        ollama_sent.starts_with("POST /v1/chat/completions "),
        "{ollama_sent}"
    );
    assert!(ollama_sent.contains(r#""model":"llama3""#), "{ollama_sent}");
}

#[tokio::test]
async fn two_plugins_with_one_route_are_stopped_by_the_kernel() {
    // The plugins share a name, because a plugin is named after its route.
    // So the kernel's own check catches this before any plugin starts, and
    // the registry never sees it.
    let Err(StartError::Invalid(problems)) = Runtime::start(with_registry(vec![
        OpenAiPlugin::new(config("deepseek", "http://127.0.0.1:1".into())),
        OpenAiPlugin::new(config("deepseek", "http://127.0.0.1:2".into())),
    ]))
    .await
    else {
        panic!("expected an invalid plugin list");
    };
    assert_eq!(problems, ["plugin \"deepseek\" is listed more than once"]);
}

/// Registers a model under "deepseek" while going by another name, so the
/// kernel's check on names can't catch the clash; only the registry can.
struct Squatter;

impl Plugin for Squatter {
    fn name(&self) -> &str {
        "squatter"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<ModelRegistry>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let model = OpenAiModel::new("http://127.0.0.1:1");
        ctx.get::<ModelRegistry>()
            .register("deepseek", Arc::new(model))?;
        Ok(())
    }
}

#[tokio::test]
async fn a_route_taken_by_another_plugin_fails_that_providers_setup() {
    let mut plugins = with_registry(vec![OpenAiPlugin::new(config(
        "deepseek",
        "http://127.0.0.1:1".into(),
    ))]);
    // Listed before the provider, so it starts first and takes the route.
    plugins.insert(1, Box::new(Squatter));

    let Err(error) = Runtime::start(plugins).await else {
        panic!("expected the plugin to fail");
    };
    assert_eq!(
        error.to_string(),
        "plugin \"deepseek\" failed to start: a model is already registered under the route \"deepseek\""
    );
}

#[tokio::test]
async fn an_empty_route_stops_the_runtime_from_starting() {
    let plugin = OpenAiPlugin::new(config("", "http://127.0.0.1:1".into()));

    let Err(StartError::Failed { source, .. }) = Runtime::start(with_registry(vec![plugin])).await
    else {
        panic!("expected the plugin to fail");
    };
    assert!(
        source.to_string().starts_with("the model route is empty"),
        "{source}"
    );
}

/// The config's retry policy reaches the model; with none, it has dsh's.
#[tokio::test]
async fn the_retry_policy_comes_from_the_config() {
    let mut limited = config("ollama", "http://127.0.0.1:1/v1".into());
    limited.retry_policy = Some(RetryPolicyConfig {
        max_retries: Some(0),
        ..RetryPolicyConfig::default()
    });
    let runtime = Runtime::start(with_registry(vec![
        OpenAiPlugin::new(limited),
        OpenAiPlugin::new(config("local", "http://127.0.0.1:1/v1".into())),
    ]))
    .await
    .expect("the runtime starts");
    let registry = runtime.get::<ModelRegistry>();
    let RetryPolicy::Normal { max_retries, .. } = registry.get("ollama").unwrap().retry_policy()
    else {
        panic!("a normal policy");
    };
    assert_eq!(max_retries, 0);
    assert_eq!(
        registry.get("local").unwrap().retry_policy(),
        RetryPolicy::default()
    );
}
