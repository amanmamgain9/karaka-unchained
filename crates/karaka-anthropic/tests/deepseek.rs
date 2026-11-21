//! `AnthropicModel` in the DeepSeek profile, against a fake server on
//! localhost, so the HTTP side is tested without DeepSeek or any network.
//!
//! The server helpers are copied from `http.rs`: each file under `tests/`
//! is its own crate.

use std::net::SocketAddr;
use std::time::Duration;

use karaka_anthropic::{AnthropicModel, DeepSeekThinking};
use karaka_model::{
    Block, Effort, ErrorKind, Message, ModelError, Request, Role, StopReason, ToolCall, ToolResult,
    ToolSchema, complete,
};
use serde_json::{Value, json};
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

/// Server-sent events framed as dsh's fixtures frame them
/// (`tests/helpers.ts:30`): the type as the `event` field, the JSON as
/// `data`, and a blank line after each.
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
    http_response("200 OK", &[("content-type", "text/event-stream")], &body)
}

/// dsh's `textEvents` (`tests/helpers.ts:26-29`).
const HELLO: &[&str] = &[
    r#"{"type":"message_start","message":{"id":"msg_1","model":"deepseek-flash","usage":{"input_tokens":12,"output_tokens":1}}}"#,
    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello 世界"}}"#,
    r#"{"type":"content_block_stop","index":0}"#,
    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
    r#"{"type":"message_stop"}"#,
];

/// A DeepSeek-profile model for the server at `address`, under the path
/// DeepSeek's own API has.
fn model(address: SocketAddr) -> AnthropicModel {
    AnthropicModel::deepseek("test-key").with_base_url(format!("http://{address}/anthropic"))
}

fn assistant(content: Vec<Block>) -> Message {
    Message {
        role: Role::Assistant,
        content,
    }
}

/// A system prompt, a tool and one round of a tool call, as in an agent's
/// tool loop.
fn tool_round() -> Request {
    let mut request = Request::new(
        "deepseek-flash",
        vec![
            Message::user("what is in a?"),
            assistant(vec![Block::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: r#"{"path":"a"}"#.into(),
            })]),
            Message {
                role: Role::User,
                content: vec![Block::ToolResult(ToolResult {
                    call_id: "call_1".into(),
                    content: String::new(),
                    is_error: false,
                })],
            },
        ],
    );
    request.system = Some("Be brief.".into());
    request.tools = vec![ToolSchema {
        name: "read".into(),
        description: "Read a file".into(),
        parameters: r#"{"type":"object"}"#.into(),
    }];
    request.effort = Some(Effort::Max);
    request
}

/// dsh's "uses the Messages endpoint, authentication, attribution and final
/// usage", without dsh's own identity headers.
#[tokio::test]
async fn a_deepseek_call_sends_what_dsh_sends() {
    let (address, server) = serve_once(streamed(HELLO)).await;

    let response = complete(
        &model(address).with_default_max_tokens(393_216),
        tool_round(),
    )
    .await
    .expect("the call works");
    assert_eq!(response.message.text(), "Hello 世界");
    assert_eq!(response.stop, StopReason::EndTurn);
    assert_eq!(response.usage.input_tokens, 12);
    assert_eq!(response.usage.output_tokens, 5);

    let sent = server.await.expect("the server ran");
    let (head, body) = sent.split_once("\r\n\r\n").expect("headers, then a body");
    let head = head.to_lowercase();
    assert!(
        head.starts_with("post /anthropic/v1/messages http/1.1\r\n"),
        "{head}"
    );
    assert!(head.contains("\r\nx-api-key: test-key\r\n"), "{head}");
    assert!(
        head.contains("\r\nanthropic-version: 2023-06-01\r\n"),
        "{head}"
    );
    assert!(
        head.contains("\r\ncontent-type: application/json\r\n"),
        "{head}"
    );
    assert!(head.contains("\r\naccept: text/event-stream\r\n"), "{head}");
    assert!(!head.contains("\r\nanthropic-beta:"), "{head}");
    assert!(!head.contains("\r\nauthorization:"), "{head}");
    assert!(!head.contains("x-deepseek-harness"), "{head}");

    let body: Value = serde_json::from_str(body).expect("the body is JSON");
    assert_eq!(
        body,
        json!({
            "model": "deepseek-flash",
            "stream": true,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "what is in a?"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "a"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": [], "is_error": false}
                ]}
            ],
            "max_tokens": 393216,
            "thinking": {"type": "enabled"},
            "output_config": {"effort": "max"},
            "system": "Be brief.",
            "tools": [
                {"name": "read", "description": "Read a file", "input_schema": {"type": "object"}}
            ]
        })
    );
}

#[tokio::test]
async fn thinking_turned_off_goes_out_as_disabled() {
    let (address, server) = serve_once(streamed(HELLO)).await;

    let model = model(address).with_deepseek_thinking(DeepSeekThinking::Disabled);
    let request = Request::new("deepseek-flash", vec![Message::user("hi")]);
    complete(&model, request).await.expect("the call works");

    let sent = server.await.expect("the server ran");
    let (_, body) = sent.split_once("\r\n\r\n").expect("headers, then a body");
    let body: Value = serde_json::from_str(body).expect("the body is JSON");
    assert_eq!(body["thinking"], json!({"type": "disabled"}));
    assert!(body.get("output_config").is_none(), "{body}");
    assert_eq!(body["max_tokens"], 256_000);
}

/// What `request` fails with when nothing would answer it. Nothing listens
/// on port 1, so a request that was sent would fail as a network error.
async fn refused_before_sending(model: AnthropicModel, request: Request) -> ModelError {
    let model = model.with_base_url("http://127.0.0.1:1");
    let error = complete(&model, request).await.expect_err("the call fails");
    assert_eq!(error.kind, ErrorKind::InvalidRequest, "{error}");
    error
}

#[tokio::test]
async fn an_effort_deepseek_lacks_fails_without_contacting_the_server() {
    let mut request = Request::new("deepseek-flash", vec![Message::user("hi")]);
    request.effort = Some(Effort::Medium);
    let error = refused_before_sending(AnthropicModel::deepseek("k"), request).await;
    assert_eq!(
        error.message,
        "DeepSeek has no effort \"medium\"; it takes low, high or max"
    );

    let mut request = Request::new("deepseek-flash", vec![Message::user("hi")]);
    request.effort = Some(Effort::High);
    let model = AnthropicModel::deepseek("k").with_deepseek_thinking(DeepSeekThinking::Disabled);
    refused_before_sending(model, request).await;
}

#[tokio::test]
async fn unanswered_tool_calls_fail_without_contacting_the_server() {
    let request = Request::new(
        "deepseek-flash",
        vec![
            assistant(vec![Block::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            })]),
            Message::user("never mind"),
        ],
    );
    refused_before_sending(AnthropicModel::deepseek("k"), request).await;
}

/// The error a failed response with `status`, `body` and `headers` comes
/// back as.
async fn failure(status: &str, headers: &[(&str, &str)], body: &str) -> ModelError {
    let (address, _server) = serve_once(http_response(status, headers, body)).await;
    let request = Request::new("deepseek-flash", vec![Message::user("hi")]);
    complete(&model(address), request)
        .await
        .expect_err("the call fails")
}

/// dsh's "maps non-2xx responses (JSON=%s)".
#[tokio::test]
async fn a_rate_limit_says_how_long_to_wait_with_or_without_json() {
    let json = r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#;
    for body in [json, "<html>busy</html>"] {
        let error = failure(
            "429 Too Many Requests",
            &[("content-type", "application/json"), ("retry-after", "3")],
            body,
        )
        .await;
        assert_eq!(error.kind, ErrorKind::RateLimited, "{body}");
        assert_eq!(error.retry_after, Some(Duration::from_secs(3)), "{body}");
    }
}

/// DeepSeek's out-of-credit error carries only a code
/// (`stream.spec.ts:12`).
#[tokio::test]
async fn an_insufficient_balance_code_is_out_of_quota() {
    let error = failure(
        "400 Bad Request",
        &[("content-type", "application/json")],
        r#"{"error":{"code":"insufficient_balance"}}"#,
    )
    .await;
    assert_eq!(error.kind, ErrorKind::Quota);
    assert_eq!(error.message, "DeepSeek's request failed (HTTP 400)");
}

/// dsh's "refuses redirects before credentials reach another origin".
#[tokio::test]
async fn a_redirect_is_not_followed() {
    let destination = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let location = format!(
        "http://{}/anthropic/v1/messages",
        destination.local_addr().expect("address")
    );
    let (address, source) = serve_once(http_response(
        "307 Temporary Redirect",
        &[("location", &location)],
        "",
    ))
    .await;

    let request = Request::new("deepseek-flash", vec![Message::user("hi")]);
    let error = complete(&model(address), request)
        .await
        .expect_err("the call fails");
    assert_eq!(error.kind, ErrorKind::Network, "{error}");
    let sent = source.await.expect("the server ran");
    assert!(sent.to_lowercase().contains("\r\nx-api-key: test-key\r\n"));

    // Nobody connected to where the redirect pointed.
    let followed = tokio::time::timeout(Duration::from_millis(200), destination.accept()).await;
    assert!(followed.is_err(), "the redirect was followed");
}

#[tokio::test]
async fn an_unsupported_block_in_the_answer_fails_the_call() {
    let (address, _server) = serve_once(streamed(&[
        HELLO[0],
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"x"}}"#,
    ]))
    .await;
    let request = Request::new("deepseek-flash", vec![Message::user("hi")]);
    let error = complete(&model(address), request)
        .await
        .expect_err("the call fails");
    assert_eq!(error.kind, ErrorKind::Malformed);
}

/// A signature DeepSeek sent comes back to DeepSeek on the next turn, to
/// the same model, as dsh sends it (`serialize.ts:31-34`).
#[tokio::test]
async fn a_signature_goes_back_on_the_next_turn() {
    let thinking = &[
        HELLO[0],
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-1"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi."}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        HELLO[4],
        HELLO[5],
    ];
    let (address, _server) = serve_once(streamed(thinking)).await;
    let first = Request::new("deepseek-flash", vec![Message::user("hi")]);
    let answer = complete(&model(address), first)
        .await
        .expect("the call works")
        .message;

    let (address, server) = serve_once(streamed(HELLO)).await;
    let second = Request::new(
        "deepseek-flash",
        vec![Message::user("hi"), answer, Message::user("again")],
    );
    complete(&model(address), second)
        .await
        .expect("the call works");

    let sent = server.await.expect("the server ran");
    let (_, body) = sent.split_once("\r\n\r\n").expect("headers, then a body");
    let body: Value = serde_json::from_str(body).expect("the body is JSON");
    assert_eq!(
        body["messages"][1],
        json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "hmm", "signature": "sig-1"},
            {"type": "text", "text": "Hi."}
        ]})
    );
}
