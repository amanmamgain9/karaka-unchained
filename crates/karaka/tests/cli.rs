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
    "KARAKA_ROUTE",
    "KARAKA_MODEL",
    "KARAKA_SYSTEM",
    "KARAKA_MAX_TOKENS",
    "KARAKA_EFFORT",
    "KARAKA_MAX_RETRIES",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_THINKING_DISPLAY",
    "ANTHROPIC_CACHE",
    "DEEPSEEK_API_KEY",
    "DEEPSEEK_BASE_URL",
    "DEEPSEEK_THINKING",
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

/// Serves one request with each of `responses`, in order, one connection
/// each. The handle gives back the requests the server received.
fn serve(responses: Vec<String>) -> (SocketAddr, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        responses
            .into_iter()
            .map(|response| {
                let (mut socket, _) = listener.accept().expect("accept");
                let request = read_request(&mut socket);
                socket.write_all(response.as_bytes()).expect("write");
                request
            })
            .collect()
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

/// DeepSeek's events for the same answer, with some thinking first. The
/// Messages format is Anthropic's, so the events are too.
const DEEPSEEK_HELLO: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_1","model":"deepseek-flash","usage":{"input_tokens":12,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Say it."}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello, world"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":1}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#,
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
fn asks_an_openai_server_under_a_route_of_its_own() {
    let (address, server) = serve_once(streamed(OPENAI_HELLO));

    let output = karaka(
        &["ask", "hi"],
        &[
            ("KARAKA_PROVIDER", "openai".into()),
            ("KARAKA_ROUTE", "deepseek".into()),
            ("KARAKA_MODEL", "deepseek-chat".into()),
            ("OPENAI_BASE_URL", format!("http://{address}")),
        ],
        "",
    );

    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(text(&output.stdout), "Hello, world\n");
    let sent = server.join().expect("the server ran");
    assert!(sent.contains(r#""model":"deepseek-chat""#), "{sent}");
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
    for args in [&["--help"][..], &[], &["chat", "hello"]] {
        let output = karaka(args, &[], "");

        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(text(&output.stdout), "", "{args:?}");
        assert!(
            text(&output.stderr).starts_with("Usage: karaka ask <question...>\n"),
            "{args:?}"
        );
    }
}

#[test]
fn asks_deepseek_in_the_messages_format() {
    let (address, server) = serve_once(streamed(DEEPSEEK_HELLO));

    let output = karaka(
        &["ask", "say", "hello"],
        &[
            ("KARAKA_PROVIDER", "deepseek".into()),
            ("DEEPSEEK_API_KEY", "test-key".into()),
            ("DEEPSEEK_BASE_URL", format!("http://{address}/anthropic")),
            ("KARAKA_SYSTEM", "Be brief.".into()),
            ("KARAKA_MAX_TOKENS", "393216".into()),
            ("KARAKA_EFFORT", "max".into()),
        ],
        "",
    );

    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(text(&output.stdout), "Hello, world\n");
    // stderr isn't a terminal here, so the reasoning isn't shown.
    assert_eq!(stderr, "stop: finished · in 12 · out 9 tokens\n");

    let sent = server.join().expect("the server ran");
    let (head, body) = sent.split_once("\r\n\r\n").expect("headers, then a body");
    let head = head.to_lowercase();
    assert!(head.starts_with("post /anthropic/v1/messages "), "{head}");
    assert!(head.contains("\r\nx-api-key: test-key\r\n"), "{head}");
    assert!(
        head.contains("\r\nanthropic-version: 2023-06-01\r\n"),
        "{head}"
    );
    assert!(!head.contains("\r\nanthropic-beta:"), "{head}");
    assert_eq!(
        body,
        concat!(
            r#"{"model":"deepseek-flash","stream":true,"#,
            r#""messages":[{"role":"user","content":[{"type":"text","text":"say hello"}]}],"#,
            r#""max_tokens":393216,"thinking":{"type":"enabled"},"#,
            r#""output_config":{"effort":"max"},"system":"Be brief."}"#
        )
    );
}

#[test]
fn deepseek_refuses_an_effort_it_lacks_before_any_call() {
    // Nothing listens on port 1; a call that went out would fail differently.
    let output = karaka(
        &["ask", "hi"],
        &[
            ("KARAKA_PROVIDER", "deepseek".into()),
            ("DEEPSEEK_API_KEY", "test-key".into()),
            ("DEEPSEEK_BASE_URL", "http://127.0.0.1:1".into()),
            ("KARAKA_EFFORT", "medium".into()),
        ],
        "",
    );

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "karaka: the provider rejected the request: DeepSeek has no effort \"medium\"; it takes low, high or max (trying again won't help)\n"
    );
}

#[test]
fn a_missing_deepseek_key_fails_before_any_call() {
    let output = karaka(
        &["ask", "hi"],
        &[("KARAKA_PROVIDER", "deepseek".into())],
        "",
    );

    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("karaka: DEEPSEEK_API_KEY is not set"),
        "{stderr}"
    );
}

/// Anthropic's events for a call to `current_time`, with no arguments.
const ANTHROPIC_CURRENT_TIME: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":12,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"current_time","input":{}}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":30}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

/// Anthropic's events for an answer of one piece of text.
fn anthropic_text(answer: &str) -> String {
    let text = json_string(answer);
    [
        r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_2","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":40,"output_tokens":1}}}"#.to_owned(),
        r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_owned(),
        format!(
            r#"event: content_block_delta
data: {{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":{text}}}}}"#
        ),
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}"#.to_owned(),
        r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}}"#.to_owned(),
        r#"event: message_stop
data: {"type":"message_stop"}"#.to_owned(),
    ]
    .map(|event| format!("{event}\n\n"))
    .concat()
}

/// `text` as a JSON string. The answers here are plain sentences, so
/// quoting them is enough.
fn json_string(text: &str) -> String {
    assert!(!text.contains(['"', '\\']), "{text}");
    format!("\"{text}\"")
}

/// The body of an HTTP request, after its headers.
fn body(request: &str) -> &str {
    request
        .split_once("\r\n\r\n")
        .expect("headers, then a body")
        .1
}

#[test]
fn chat_runs_the_clock_tool_and_keeps_the_conversation() {
    let (address, server) = serve(vec![
        streamed(ANTHROPIC_CURRENT_TIME),
        streamed(&anthropic_text("It is morning in UTC.")),
        streamed(&anthropic_text("You asked for the time.")),
    ]);

    let output = karaka(
        &["chat"],
        &[
            ("ANTHROPIC_API_KEY", "test-key".into()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
            ("KARAKA_SYSTEM", "Be brief.".into()),
        ],
        "what time is it?\n\nwhat did I ask?\n",
    );

    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(stderr, "");
    let stdout = text(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{stdout}");
    let time = lines[0]
        .strip_prefix("[current_time gave: ")
        .and_then(|line| line.strip_suffix("]"))
        .expect("a line for the tool call");
    assert!(time.ends_with(" GMT"), "{time}");
    assert_eq!(lines[1], "It is morning in UTC.");
    assert_eq!(lines[2], "You asked for the time.");

    let sent = server.join().expect("the server ran");
    // The first request offers the tool.
    let first = body(&sent[0]);
    assert!(first.contains(r#""name":"current_time""#), "{first}");
    assert!(first.contains(r#""Be brief.""#), "{first}");
    // The second carries the tool's answer back.
    let second = body(&sent[1]);
    assert!(second.contains(r#""tool_use_id":"toolu_1""#), "{second}");
    assert!(second.contains(time), "{second}");
    // The third holds the whole conversation, the blank line skipped.
    let third = body(&sent[2]);
    for said in [
        "what time is it?",
        "It is morning in UTC.",
        "what did I ask?",
    ] {
        assert!(third.contains(said), "{said}: {third}");
    }
}

#[test]
fn chat_reports_a_failed_turn_and_carries_on() {
    let (address, server) = serve(vec![
        http_response(
            "429 Too Many Requests",
            &[("content-type", "application/json")],
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"Slow down."}}"#,
        ),
        streamed(&anthropic_text("Hello again.")),
    ]);

    let output = karaka(
        &["chat"],
        &[
            ("ANTHROPIC_API_KEY", "test-key".into()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
            // Off, so the failure reaches the chat.
            ("KARAKA_MAX_RETRIES", "0".into()),
        ],
        "hi\nhi again\n",
    );

    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(stderr, "karaka: Slow down. (RATE_LIMIT)\n");
    assert_eq!(text(&output.stdout), "Hello again.\n");
    let sent = server.join().expect("the server ran");
    assert_eq!(sent.len(), 2);
}

#[test]
fn chat_needs_the_same_settings_as_ask() {
    let output = karaka(&["chat"], &[], "hi\n");

    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("karaka: ANTHROPIC_API_KEY is not set"),
        "{stderr}"
    );
}

/// A rate limit is retried before the turn gives up: the server refuses
/// once, the chat waits about half a second, as dsh's default policy
/// does, and the answer comes on the second request.
#[test]
fn chat_retries_a_rate_limit_and_answers() {
    let (address, server) = serve(vec![
        http_response(
            "429 Too Many Requests",
            &[("content-type", "application/json")],
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"Slow down."}}"#,
        ),
        streamed(&anthropic_text("Hello after a wait.")),
    ]);

    let output = karaka(
        &["chat"],
        &[
            ("ANTHROPIC_API_KEY", "test-key".into()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
        ],
        "hi\n",
    );

    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(stderr, "");
    assert_eq!(text(&output.stdout), "Hello after a wait.\n");
    let sent = server.join().expect("the server ran");
    assert_eq!(sent.len(), 2);
    // Both requests carried the same conversation.
    assert_eq!(body(&sent[0]), body(&sent[1]));
}
