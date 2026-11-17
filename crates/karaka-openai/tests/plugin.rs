//! `OpenAiPlugin` started in a Karaka runtime, and the model it provides
//! called against a fake server on localhost.
//!
//! The server helpers are copied from `http.rs`: each file under `tests/` is
//! its own crate, and sharing code between them needs a module that both
//! include, which isn't worth it for three small functions.

use std::net::SocketAddr;
use std::sync::Arc;

use karaka_kernel::{Runtime, StartError};
use karaka_model::{Message, Model, Request, complete};
use karaka_openai::{OpenAiConfig, OpenAiPlugin};
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

#[tokio::test]
async fn the_plugin_provides_a_model_that_answers() {
    let (address, server) = serve_once(streamed(HELLO)).await;
    let plugin = OpenAiPlugin::new(OpenAiConfig {
        base_url: format!("http://{address}/v1"),
        api_key: Some("test-key".into()),
    });

    let runtime = Runtime::start(vec![Box::new(plugin)])
        .await
        .expect("the runtime starts");
    assert_eq!(runtime.plugins(), ["openai"]);
    let model: Arc<dyn Model> = runtime.get::<dyn Model>();

    let request = Request::new("gpt-test", vec![Message::user("hi")]);
    let response = complete(model.as_ref(), request)
        .await
        .expect("the call works");
    assert_eq!(response.message.text(), "Hello");

    // The config reached the model: its base URL and its key.
    let sent = server.await.expect("the server ran");
    let head = sent
        .split_once("\r\n\r\n")
        .expect("headers")
        .0
        .to_lowercase();
    assert!(head.starts_with("post /v1/chat/completions "), "{head}");
    assert!(
        head.contains("\r\nauthorization: bearer test-key\r\n"),
        "{head}"
    );
}

#[tokio::test]
async fn an_empty_base_url_stops_the_runtime_from_starting() {
    let plugin = OpenAiPlugin::new(OpenAiConfig {
        base_url: String::new(),
        api_key: None,
    });

    let Err(StartError::Failed {
        plugin: name,
        source,
    }) = Runtime::start(vec![Box::new(plugin)]).await
    else {
        panic!("expected the plugin to fail");
    };
    assert_eq!(name, "openai");
    assert_eq!(
        source.to_string(),
        "the OpenAI-compatible base URL is empty"
    );
}
