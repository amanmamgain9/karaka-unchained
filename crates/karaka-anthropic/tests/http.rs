//! `AnthropicModel` against a fake server on localhost, so the HTTP side is
//! tested without the real API or any network.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::StreamExt;
use karaka_anthropic::{AnthropicModel, ThinkingDisplay};
use karaka_model::{
    ErrorKind, Event, FinishReason, Message, Model, ModelError, Request, ToolSchema, complete,
};
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

/// Server-sent events framed as Anthropic frames them: the type as the
/// `event` field, the JSON as `data`, and a blank line after each.
fn sse(events: &[&str]) -> String {
    events
        .iter()
        .map(|data| {
            let event: Value = serde_json::from_str(data).expect("valid JSON");
            format!(
                "event: {}\ndata: {data}\n\n",
                event["type"].as_str().expect("a type")
            )
        })
        .collect()
}

const START: &str = r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":20,"output_tokens":1}}}"#;
const TEXT_START: &str =
    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
const HEL: &str =
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#;
const LO: &str =
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#;

fn model(address: SocketAddr) -> AnthropicModel {
    AnthropicModel::new("test-key").with_base_url(format!("http://{address}/"))
}

fn request() -> Request {
    Request::new("claude-opus-5-5", vec![Message::user("hi")])
}

#[tokio::test]
async fn a_streamed_text_answer() {
    let body = sse(&[
        START,
        TEXT_START,
        r#"{"type":"ping"}"#,
        HEL,
        LO,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    let (address, server) = serve_once(http_response(
        "200 OK",
        &[("content-type", "text/event-stream")],
        &body,
    ))
    .await;

    let response = complete(&model(address), request())
        .await
        .expect("the call works");
    assert_eq!(response.message.text(), "Hello");
    assert_eq!(response.finish, FinishReason::Stop);
    assert_eq!(response.usage.unwrap().input_tokens, 20);
    assert_eq!(response.usage.unwrap().output_tokens, 5);

    let sent = server.await.expect("the server ran");
    let (head, body) = sent.split_once("\r\n\r\n").expect("headers, then a body");
    let head = head.to_lowercase();
    assert!(head.starts_with("post /v1/messages "), "{head}");
    assert!(head.contains("\r\nx-api-key: test-key\r\n"), "{head}");
    assert!(
        head.contains("\r\nanthropic-version: 2026-10-21\r\n"),
        "{head}"
    );
    assert!(
        head.contains("\r\ncontent-type: application/json\r\n"),
        "{head}"
    );
    assert!(head.contains("\r\naccept: text/event-stream\r\n"), "{head}");
    // No thinking setting, so no beta either.
    assert!(!head.contains("\r\nanthropic-beta:"), "{head}");
    let body: Value = serde_json::from_str(body).expect("the body is JSON");
    assert_eq!(body["stream"], true);
    assert_eq!(body["model"], "claude-opus-5-5");
}

#[tokio::test]
async fn thinking_sends_the_beta_its_block_binding_needs() {
    let body = sse(&[
        START,
        TEXT_START,
        HEL,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    let (address, server) = serve_once(http_response(
        "200 OK",
        &[("content-type", "text/event-stream")],
        &body,
    ))
    .await;

    let model = model(address).with_thinking_display(ThinkingDisplay::Summarized);
    complete(&model, request()).await.expect("the call works");

    let sent = server.await.expect("the server ran");
    let (head, body) = sent.split_once("\r\n\r\n").expect("headers, then a body");
    assert!(
        head.to_lowercase()
            .contains("\r\nanthropic-beta: thinking-binding-controls-2026-08-01\r\n"),
        "{head}"
    );
    let body: Value = serde_json::from_str(body).expect("the body is JSON");
    assert_eq!(
        body["thinking"]["block_binding"]["prefix_mismatch_behavior"],
        "drop_block"
    );
}

#[tokio::test]
async fn a_rate_limit_says_how_long_to_wait() {
    let (address, _server) = serve_once(http_response(
        "429 Too Many Requests",
        &[("content-type", "application/json"), ("retry-after", "7")],
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"Slow down."}}"#,
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
async fn a_failure_without_anthropics_json_goes_by_the_status() {
    let (address, _server) = serve_once(http_response(
        "403 Forbidden",
        &[("content-type", "text/html")],
        "<h1>no</h1>",
    ))
    .await;

    let error = complete(&model(address), request())
        .await
        .expect_err("the call fails");
    assert_eq!(error.kind, ErrorKind::Auth);
    assert_eq!(error.message, "HTTP 403 Forbidden: <h1>no</h1>");
    assert_eq!(error.retry_after, None);
}

#[tokio::test]
async fn a_stream_that_stops_early_is_a_network_error() {
    let (address, _server) = serve_once(http_response(
        "200 OK",
        &[("content-type", "text/event-stream")],
        &sse(&[START, TEXT_START, HEL]),
    ))
    .await;

    let mut stream = model(address).stream(request());
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    // The block's start and the delta that arrived are passed on, then the
    // failure ends the stream.
    assert_eq!(items.len(), 3, "{items:?}");
    let error = items[2].clone().expect_err("the last item is the failure");
    assert_eq!(error.kind, ErrorKind::Network);
}

#[tokio::test]
async fn broken_tool_parameters_fail_without_contacting_the_server() {
    let mut request = request();
    request.tools.push(ToolSchema {
        name: "clock".into(),
        description: "Tells the time.".into(),
        parameters: "not json".into(),
    });
    // Nothing listens on port 1. Were the request sent, the error would be
    // a network one instead.
    let model = AnthropicModel::new("test-key").with_base_url("http://127.0.0.1:1");

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

/// The error a failed response with `status`, `body` and `headers` comes
/// back as.
async fn failure(status: &str, headers: &[(&str, &str)], body: &str) -> ModelError {
    let (address, _server) = serve_once(http_response(status, headers, body)).await;
    complete(&model(address), request())
        .await
        .expect_err("the call fails")
}

#[tokio::test]
async fn a_prompt_that_is_too_long_is_a_context_overflow() {
    let error = failure(
        "400 Bad Request",
        &[("content-type", "application/json")],
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 213462 tokens > 200000 maximum"}}"#,
    )
    .await;
    assert_eq!(error.kind, ErrorKind::ContextWindowExceeded);
    assert_eq!(
        error.message,
        "prompt is too long: 213462 tokens > 200000 maximum"
    );
}

#[tokio::test]
async fn another_invalid_request_is_not_a_context_overflow() {
    let error = failure(
        "400 Bad Request",
        &[("content-type", "application/json")],
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: 200000 > 128000, which is the maximum allowed number of output tokens for claude-opus-5-5"}}"#,
    )
    .await;
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
}

#[tokio::test]
async fn a_402_is_out_of_quota() {
    let error = failure("402 Payment Required", &[], "").await;
    assert_eq!(error.kind, ErrorKind::Quota);
    assert!(!error.kind.is_retryable());
    assert_eq!(error.message, "HTTP 402 Payment Required");
}

#[tokio::test]
async fn a_billing_error_is_out_of_quota() {
    let error = failure(
        "400 Bad Request",
        &[("content-type", "application/json")],
        r#"{"type":"error","error":{"type":"billing_error","message":"Your credit balance is too low to access the Anthropic API."}}"#,
    )
    .await;
    assert_eq!(error.kind, ErrorKind::Quota);
}

#[tokio::test]
async fn retry_after_can_have_a_decimal_part() {
    let error = failure(
        "529 Overloaded",
        &[("content-type", "application/json"), ("retry-after", "1.5")],
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    )
    .await;
    assert_eq!(error.kind, ErrorKind::Unavailable);
    assert_eq!(error.retry_after, Some(Duration::from_millis(1500)));
}

#[tokio::test]
async fn a_retry_after_of_zero_asks_for_no_wait() {
    let error = failure(
        "429 Too Many Requests",
        &[("content-type", "application/json"), ("retry-after", "0")],
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"Slow down."}}"#,
    )
    .await;
    assert_eq!(error.retry_after, None);
}

#[tokio::test]
async fn a_refusal_after_some_text_ends_the_stream_in_an_error() {
    let body = sse(&[
        START,
        TEXT_START,
        HEL,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_sequence":null,"stop_details":{"type":"refusal","category":"bio","explanation":null}},"usage":{"output_tokens":5}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    let (address, _server) = serve_once(http_response(
        "200 OK",
        &[("content-type", "text/event-stream")],
        &body,
    ))
    .await;

    let mut stream = model(address).stream(request());
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    // The block's start, its delta and its end went out before the
    // refusal arrived.
    assert_eq!(items.len(), 4, "{items:?}");
    assert!(matches!(items[0], Ok(Event::BlockStart { .. })));
    assert!(matches!(items[1], Ok(Event::TextDelta { .. })));
    assert!(matches!(items[2], Ok(Event::BlockEnd { .. })));
    let error = items[3].clone().expect_err("the last item is the refusal");
    assert_eq!(error.kind, ErrorKind::Refused);
    let category = error.refusal.and_then(|refusal| refusal.category);
    assert_eq!(category.as_deref(), Some("bio"));
}
