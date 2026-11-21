//! The built `karaka` binary, run as a separate process against fake servers
//! on localhost, so the whole path from command line to printed answer is
//! tested without a real API or any network.
//!
//! The process gets only the environment variables each test gives it. The
//! real ones, such as an API key in the developer's shell, are removed.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::thread::{self, JoinHandle};

/// Every variable the binary reads. Each test starts with none of them set.
const VARIABLES: &[&str] = &[
    "KARAKA_PROVIDER",
    "KARAKA_MODEL",
    "KARAKA_SYSTEM",
    "KARAKA_MAX_TOKENS",
    "KARAKA_EFFORT",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_THINKING_DISPLAY",
    "ANTHROPIC_CACHE",
    "OPENAI_BASE_URL",
    "OPENAI_API_KEY",
];

/// Runs `karaka` with `args`, `env` as its only settings, and `stdin` as its
/// input.
fn karaka(args: &[&str], env: &[(&str, String)], stdin: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_karaka"));
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in VARIABLES {
        command.env_remove(name);
    }
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("the binary runs");
    // Taking stdin out of `child` and dropping it closes the pipe, so the
    // binary sees the end of its input.
    let mut input = child.stdin.take().expect("stdin is piped");
    input.write_all(stdin.as_bytes()).expect("write stdin");
    drop(input);
    child.wait_with_output().expect("the binary finishes")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("the output is UTF-8")
}

/// Serves one request with `response`, written out exactly as given.
/// The handle gives back the request the server received.
///
/// A plain thread rather than an async task: the binary is another process,
/// so there's nothing here to share an executor with.
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

/// Anthropic's events for the answer "Hello, world", in two pieces, with
/// some of the input read from the cache.
const ANTHROPIC_HELLO: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":12,"cache_read_input_tokens":100,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello, "}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":34}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

/// The same answer from an OpenAI-compatible server.
const OPENAI_HELLO: &str = concat!(
    r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"Hello, "},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"content":"world"},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    "\n\n",
    r#"data: {"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":5}}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

fn streamed(body: &str) -> String {
    http_response("200 OK", &[("content-type", "text/event-stream")], body)
}

#[test]
fn asks_anthropic_and_prints_the_answer() {
    let (address, server) = serve_once(streamed(ANTHROPIC_HELLO));

    let output = karaka(
        &["ask", "say", "hello"],
        &[
            ("ANTHROPIC_API_KEY", "test-key".into()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
            ("KARAKA_SYSTEM", "Be brief.".into()),
            ("KARAKA_MAX_TOKENS", "200".into()),
            ("KARAKA_EFFORT", "medium".into()),
        ],
        "",
    );

    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(text(&output.stdout), "Hello, world\n");
    assert_eq!(
        stderr,
        "stop: finished · in 12 (+100 cached) · out 34 tokens\n"
    );

    // The question and the settings made it into the request.
    let sent = server.join().expect("the server ran");
    let (head, body) = sent.split_once("\r\n\r\n").expect("headers, then a body");
    assert!(
        head.to_lowercase().contains("\r\nx-api-key: test-key\r\n"),
        "{head}"
    );
    assert!(body.contains(r#""model":"claude-opus-5-5""#), "{body}");
    assert!(body.contains(r#""say hello""#), "{body}");
    assert!(body.contains(r#""Be brief.""#), "{body}");
    assert!(body.contains(r#""max_tokens":200"#), "{body}");
    assert!(
        body.contains(r#""output_config":{"effort":"medium"}"#),
        "{body}"
    );
    // The binary asks for summaries, and caches by default.
    assert!(body.contains(r#""display":"summarized""#), "{body}");
    assert!(
        body.contains(r#""cache_control":{"type":"ephemeral"}"#),
        "{body}"
    );
    assert!(
        head.to_lowercase()
            .contains("\r\nanthropic-beta: thinking-binding-controls-2026-08-01\r\n"),
        "{head}"
    );
}

#[test]
fn asks_an_openai_server_a_question_read_from_stdin() {
    let (address, server) = serve_once(streamed(OPENAI_HELLO));

    let output = karaka(
        &["ask", "-"],
        &[
            ("KARAKA_PROVIDER", "openai".into()),
            ("KARAKA_MODEL", "llama-test".into()),
            ("OPENAI_BASE_URL", format!("http://{address}/v1")),
        ],
        "say hello\n",
    );

    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(text(&output.stdout), "Hello, world\n");
    assert_eq!(stderr, "stop: finished · in 20 · out 5 tokens\n");

    let sent = server.join().expect("the server ran");
    let (head, body) = sent.split_once("\r\n\r\n").expect("headers, then a body");
    let head = head.to_lowercase();
    assert!(head.starts_with("post /v1/chat/completions "), "{head}");
    assert!(!head.contains("\r\nauthorization:"), "{head}");
    assert!(body.contains(r#""model":"llama-test""#), "{body}");
    // The newline that ended the input is trimmed off.
    assert!(body.contains(r#""say hello""#), "{body}");
}

#[test]
fn a_model_error_says_whether_to_try_again() {
    let (address, _server) = serve_once(http_response(
        "429 Too Many Requests",
        &[("content-type", "application/json"), ("retry-after", "7")],
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"Slow down."}}"#,
    ));

    let output = karaka(
        &["ask", "hi"],
        &[
            ("ANTHROPIC_API_KEY", "test-key".into()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
        ],
        "",
    );

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "karaka: rate limited: Slow down. (trying again may help; the provider asked to wait 7s)\n"
    );
}

#[test]
fn a_missing_key_fails_before_any_call() {
    let output = karaka(&["ask", "hi"], &[], "");

    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("karaka: ANTHROPIC_API_KEY is not set"),
        "{stderr}"
    );
}

#[test]
fn an_empty_question_is_rejected() {
    let output = karaka(
        &["ask"],
        &[("ANTHROPIC_API_KEY", "test-key".into())],
        "  \n",
    );

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stderr), "karaka: the question is empty\n");
}

#[test]
fn help_prints_the_usage_and_exits_with_2() {
    for args in [&["--help"][..], &[], &["chat"]] {
        let output = karaka(args, &[], "");

        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(text(&output.stdout), "", "{args:?}");
        assert!(
            text(&output.stderr).starts_with("Usage: karaka ask <question...>\n"),
            "{args:?}"
        );
    }
}
