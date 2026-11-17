//! `OpenAiModel` against a fake server on localhost, so the HTTP side is
//! tested without a real API or any network.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::StreamExt;
use karaka_model::{
    ErrorKind, Event, Message, Model, ModelError, Request, StopReason, ToolSchema, complete,
};
use karaka_openai::OpenAiModel;
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

/// An HTTP/1.1 response that closes the connection after `body`.
fn http_response(status: &str, headers: &[(&str, &str)], body: &str) -> String {
    let mut response = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    ));
    response
}

/// Server-sent events framed as OpenAI frames them: only `data`, with a
/// blank line after each.
fn sse(chunks: &[&str]) -> String {
    chunks
        .iter()
        .map(|data| format!("data: {data}\n\n"))
        .collect()
}

/// A streamed 200 response holding `chunks`.
fn streamed(chunks: &[&str]) -> String {
    http_response(
        "200 OK",
        &[("content-type", "text/event-stream")],
        &sse(chunks),
    )
}

const HEL: &str = r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]}"#;
const LO: &str = r#"{"choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}"#;
const STOP: &str = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
const USAGE: &str = r#"{"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":5}}"#;

fn model(address: SocketAddr) -> OpenAiModel {
    OpenAiModel::new(format!("http://{address}/v1")).with_api_key("test-key")
}

fn request() -> Request {
    Request::new("gpt-test", vec![Message::user("hi")])
}

/// Every item a stream yields, up to its end.
async fn drain(model: &OpenAiModel) -> Vec<Result<Event, ModelError>> {
    let mut stream = model.stream(request());
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    items
}

#[tokio::test]
async fn a_streamed_text_answer() {
    let (address, server) = serve_once(streamed(&[HEL, LO, STOP, USAGE, "[DONE]"])).await;

    let response = complete(&model(address), request())
        .await
        .expect("the call works");
    assert_eq!(response.message.text(), "Hello");
    assert_eq!(response.stop, StopReason::EndTurn);
    assert_eq!(response.usage.input_tokens, 20);
    assert_eq!(response.usage.output_tokens, 5);

    let sent = server.await.expect("the server ran");
    let (head, body) = sent.split_once("\r\n\r\n").expect("headers, then a body");
    let head = head.to_lowercase();
    assert!(head.starts_with("post /v1/chat/completions "), "{head}");
    assert!(
        head.contains("\r\nauthorization: bearer test-key\r\n"),
        "{head}"
    );
    assert!(
        head.contains("\r\ncontent-type: application/json\r\n"),
        "{head}"
    );
    assert!(head.contains("\r\naccept: text/event-stream\r\n"), "{head}");
    let body: Value = serde_json::from_str(body).expect("the body is JSON");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["model"], "gpt-test");
}

#[tokio::test]
async fn without_an_api_key_no_authorization_is_sent() {
    let (address, server) = serve_once(streamed(&[HEL, STOP, "[DONE]"])).await;

    let model = OpenAiModel::new(format!("http://{address}/v1"));
    complete(&model, request()).await.expect("the call works");

    let sent = server.await.expect("the server ran").to_lowercase();
    assert!(!sent.contains("\r\nauthorization:"), "{sent}");
}

#[tokio::test]
async fn a_body_that_ends_after_the_finish_reason_still_finishes() {
    // Some local servers close the body without `[DONE]`.
    let (address, _server) = serve_once(streamed(&[HEL, LO, STOP])).await;

    let response = complete(&model(address), request())
        .await
        .expect("the call works");
    assert_eq!(response.message.text(), "Hello");
    assert_eq!(response.stop, StopReason::EndTurn);
}

#[tokio::test]
async fn a_rate_limit_says_how_long_to_wait() {
    let (address, _server) = serve_once(http_response(
        "429 Too Many Requests",
        &[("content-type", "application/json"), ("retry-after", "7")],
        r#"{"error":{"message":"Slow down.","type":"requests","code":"rate_limit_exceeded"}}"#,
    ))
    .await;

    let error = complete(&model(address), request())
        .await
        .expect_err("the call fails");
    assert_eq!(error.kind, ErrorKind::RateLimited);
    assert_eq!(error.message, "Slow down.");
    assert_eq!(error.retry_after, Some(Duration::from_secs(7)));
}

#[tokio::test]
async fn running_out_of_quota_is_not_a_rate_limit() {
    let (address, _server) = serve_once(http_response(
        "429 Too Many Requests",
        &[("content-type", "application/json")],
        r#"{"error":{"message":"You exceeded your current quota.","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#,
    ))
    .await;

    let error = complete(&model(address), request())
        .await
        .expect_err("the call fails");
    assert_eq!(error.kind, ErrorKind::Auth);
    assert!(!error.kind.is_retryable());
    assert_eq!(error.message, "You exceeded your current quota.");
}

#[tokio::test]
async fn a_failure_without_the_error_json_goes_by_the_status() {
    let (address, _server) = serve_once(http_response(
        "422 Unprocessable Entity",
        &[("content-type", "text/plain")],
        "bad request",
    ))
    .await;

    let error = complete(&model(address), request())
        .await
        .expect_err("the call fails");
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    assert_eq!(error.message, "HTTP 422 Unprocessable Entity: bad request");
    assert_eq!(error.retry_after, None);
}

#[tokio::test]
async fn a_stream_cut_off_before_the_finish_reason_is_a_network_error() {
    let (address, _server) = serve_once(streamed(&[HEL])).await;

    let items = drain(&model(address)).await;
    // The delta that arrived is passed on, then the failure ends the stream.
    assert_eq!(items.len(), 2, "{items:?}");
    let error = items[1].clone().expect_err("the last item is the failure");
    assert_eq!(error.kind, ErrorKind::Network);
}

#[tokio::test]
async fn nothing_after_done_is_read() {
    let garbage = "not json";
    let (address, _server) = serve_once(streamed(&[HEL, STOP, "[DONE]", garbage])).await;

    let items = drain(&model(address)).await;
    assert!(
        items.iter().all(Result::is_ok),
        "the garbage after [DONE] was read: {items:?}"
    );
}

#[tokio::test]
async fn broken_tool_parameters_fail_without_contacting_a_server() {
    let mut request = request();
    request.tools.push(ToolSchema {
        name: "clock".into(),
        description: "Tells the time.".into(),
        parameters: "not json".into(),
    });
    // Nothing listens on port 1. Were the request sent, the error would be
    // a network one instead.
    let model = OpenAiModel::new("http://127.0.0.1:1/v1");

    let error = complete(&model, request).await.expect_err("the call fails");
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
}

#[tokio::test]
async fn nothing_is_sent_until_the_stream_is_polled() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let mut stream = model(address).stream(request());

    let early = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
    assert!(early.is_err(), "connected before the first poll");

    // Polling does connect, so the check above could have seen it.
    let call = tokio::spawn(async move { stream.next().await });
    let (socket, _) = listener.accept().await.expect("connected once polled");
    drop(socket);
    let item = call.await.expect("the call ran").expect("one item");
    assert_eq!(
        item.expect_err("the server hung up").kind,
        ErrorKind::Network
    );
}
