//! The built `karaka` binary, run as a separate process against fake servers
//! on localhost, so the whole path from command line to printed answer is
//! tested without a real API or any network.
//!
//! The process gets only the environment variables each test gives it. The
//! real ones, such as an API key in the developer's shell, are removed.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use serde_json::Value;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use karaka_session::check_assistant_streams;
use karaka_session_jsonl::{JsonlConfig, JsonlPersistence};
use karaka_session_persistence::{SessionAccess, SessionPersistence};

/// Every variable the binary reads. Each test starts with none of them set.
const VARIABLES: &[&str] = &[
    "KARAKA_PROVIDER",
    "KARAKA_ROUTE",
    "KARAKA_MODEL",
    "KARAKA_SYSTEM",
    "KARAKA_MAX_TOKENS",
    "KARAKA_EFFORT",
    "KARAKA_MAX_RETRIES",
    // Removed too, so no test ever reads or writes a real session store.
    "KARAKA_SESSIONS",
    "KARAKA_SESSION",
    "KARAKA_PRESETS",
    "KARAKA_PRESET",
    "KARAKA_MCP_SERVERS",
    "KARAKA_PLUGINS",
    "KARAKA_APPLICATION_ID",
    "KARAKA_TENANT_ID",
    "KARAKA_USER_ID",
    // The credential references the application tests' plugin files name.
    "APP_CHAT",
    "APP_TOOLS",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_THINKING_DISPLAY",
    "ANTHROPIC_CACHE",
    "DEEPSEEK_API_KEY",
    "DEEPSEEK_BASE_URL",
    "DEEPSEEK_THINKING",
    "OPENAI_BASE_URL",
    "OPENAI_API_KEY",
    // The skill roots' homes. Each test points them at a directory that
    // doesn't exist, unless it names its own, so no test ever reads the
    // real `~/.dsh` or `~/.agents`.
    "DSH_HOME",
    "DSH_AGENTS_HOME",
    "DSH_BUNDLED_SKILL_DIR",
];

/// Runs `karaka` with `args`, `env` as its only settings, and `stdin` as its
/// input.
///
/// A `chat` stores its sessions: in a fresh temporary directory, unless the
/// test names one with `KARAKA_SESSIONS`. Every log in that fresh directory
/// is then held to dsh's saved-stream rule, so each answer the binary logs
/// is what its stream assembles to, and the directory is removed.
fn karaka(args: &[&str], env: &[(&str, String)], stdin: &str) -> Output {
    let stored = (args.first() == Some(&"chat")
        && !env.iter().any(|(name, _)| *name == "KARAKA_SESSIONS"))
    .then(fresh_sessions);
    let mut env = env.to_vec();
    if let Some(root) = &stored {
        env.push(("KARAKA_SESSIONS", root.display().to_string()));
    }
    let output = run(args, &env, stdin);
    if let Some(root) = stored {
        check_stored_streams(&root);
        let _ = std::fs::remove_dir_all(&root);
    }
    output
}

/// A temporary directory of this test's own for a chat's sessions.
fn fresh_sessions() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("karaka-cli-chat-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

/// Reads every session stored under `root` and checks each against its
/// streams, failing the test on the first that disagrees.
fn check_stored_streams(root: &Path) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime to read the sessions");
    runtime.block_on(async {
        let persistence = JsonlPersistence::new(JsonlConfig::new(root)).expect("the root reads");
        for snapshot in persistence.list().await.expect("the sessions list") {
            let id = snapshot.header.id;
            let handle = persistence
                .open(&id, SessionAccess::Read)
                .await
                .expect("the session opens");
            let events = handle.read(0, None).await.expect("the session reads");
            handle.close().await.expect("the handle closes");
            if let Err(error) = check_assistant_streams(&events) {
                panic!("session \"{id}\" disagrees with its streams: {error}");
            }
        }
    });
}

/// Runs the binary with `env` as its only settings.
fn run(args: &[&str], env: &[(&str, String)], stdin: &str) -> Output {
    run_in(None, args, env, stdin)
}

/// Runs the binary in `cwd`, or in the test's own directory, with `env` as
/// its only settings.
fn run_in(cwd: Option<&Path>, args: &[&str], env: &[(&str, String)], stdin: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_karaka"));
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    for name in VARIABLES {
        command.env_remove(name);
    }
    let nowhere = std::env::temp_dir().join(format!("karaka-cli-no-skills-{}", std::process::id()));
    for name in ["DSH_HOME", "DSH_AGENTS_HOME", "DSH_BUNDLED_SKILL_DIR"] {
        command.env(name, nowhere.join(name));
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
    // The identity line opens the prompt, then `KARAKA_SYSTEM`, the persona.
    assert!(
        body.contains(r#""You are an AI agent powered by Karaka.\n\nBe brief.""#),
        "{body}"
    );
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
            // Off: this checks the error line, not the retry.
            ("KARAKA_MAX_RETRIES", "0".into()),
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
            text(&output.stderr).starts_with("Usage: karaka ask [options] <question...>\n"),
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
    // After the question, the runtime context: the session's working
    // directory, which for `ask` is the process's own, then the approval
    // policy. The tools are the legacy `ask_user_question`, `skill`,
    // `todo_write`, parallel work allowed, and `working_directory`. There
    // are no skills, so there is no catalog.
    let directory = std::env::current_dir().unwrap();
    let context = format!(
        "Current runtime context. This snapshot supersedes earlier runtime-context \
         snapshots.\n\nCurrent working directory: {}.\n\n{}",
        serde_json::to_string(directory.to_str().unwrap()).unwrap(),
        karaka_user_approval::ASK_SENTENCE
    );
    assert_eq!(
        body,
        format!(
            concat!(
                r#"{{"model":"deepseek-flash","stream":true,"#,
                r#""messages":[{{"role":"user","content":[{{"type":"text","text":"say hello"}},"#,
                r#"{{"type":"text","text":{context}}}]}}],"#,
                r#""max_tokens":393216,"thinking":{{"type":"enabled"}},"#,
                r#""output_config":{{"effort":"max"}},"#,
                r#""system":"You are an AI agent powered by Karaka.\n\nBe brief.","#,
                r#""tools":[{{"name":"ask_user_question","description":{ask},"#,
                r#""input_schema":{ask_parameters}}},"#,
                r#"{{"name":"skill","description":{skill},"input_schema":{skill_parameters}}},"#,
                r#"{{"name":"todo_write","description":{todo},"#,
                r#""input_schema":{{"type":"object","properties":{{"todos":{{"type":"array","#,
                r#""description":"The COMPLETE task list, replacing any previous list.","#,
                r#""items":{{"type":"object","additionalProperties":false,"properties":{{"#,
                r#""content":{{"type":"string","#,
                r#""description":"What the task is — a short imperative line."}},"#,
                r#""status":{{"type":"string","#,
                r#""description":"pending (not started) | in_progress (now) | completed (done).","#,
                r#""enum":["pending","in_progress","completed"]}}}},"#,
                r#""required":["content","status"]}}}}}},"required":["todos"]}}}},"#,
                r#"{{"name":"working_directory","description":"Read the current "#,
                r#"working directory, or change it with cd. Relative paths use the current "#,
                r#"directory. Existing shells and running processes keep their own "#,
                r#"directories.","input_schema":{{"type":"object","properties":{{"cd":"#,
                r#"{{"type":"string","description":"Existing directory to enter. Omit to "#,
                r#"read the current directory."}}}}}}}}]}}"#
            ),
            context = serde_json::to_string(&context).unwrap(),
            todo = serde_json::to_string(&karaka_tool_todo::description(true)).unwrap(),
            ask = serde_json::to_string(karaka_tool_ask_user::LEGACY_DESCRIPTION).unwrap(),
            skill = serde_json::to_string(karaka_tool_skill::DESCRIPTION).unwrap(),
            skill_parameters = karaka_tool_skill::PARAMETERS,
            ask_parameters =
                karaka_tool_ask_user::parameters(karaka_tool_ask_user::Mode::Legacy, 120)
        )
    );
}

// A skill under the project's `.agents/skills` is in the catalog after the
// question, and `/demo` in the question loads it at once, after the
// catalog. The project is the directory `ask` runs in.
#[test]
fn ask_shows_the_projects_skills_and_loads_an_invoked_one() {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let project = std::env::temp_dir().join(format!(
        "karaka-cli-skills-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let skill = project.join(".agents/skills/demo");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo\ndescription: Say hello the demo way.\n---\n\nSay hello, world.\n",
    )
    .unwrap();
    let (address, server) = serve_once(streamed(DEEPSEEK_HELLO));

    let output = run_in(
        Some(&project),
        &["ask", "/demo", "please"],
        &[
            ("KARAKA_PROVIDER", "deepseek".into()),
            ("DEEPSEEK_API_KEY", "test-key".into()),
            ("DEEPSEEK_BASE_URL", format!("http://{address}/anthropic")),
        ],
        "",
    );

    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(text(&output.stdout), "Hello, world\n");
    let sent = server.join().expect("the server ran");
    let (_, body) = sent.split_once("\r\n\r\n").expect("headers, then a body");
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    let texts: Vec<&str> = body["messages"][0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts.len(), 4, "{texts:?}");
    assert_eq!(texts[0], "/demo please");
    assert!(
        texts[1]
            .contains("<available_skills>\n- `demo`: Say hello the demo way.\n</available_skills>"),
        "{}",
        texts[1]
    );
    let base = std::fs::canonicalize(&skill).unwrap();
    assert_eq!(
        texts[2],
        format!(
            "<skill_content name=\"demo\">\n<skill_resources>\nBase directory for this skill: {}\n\
             Resolve relative paths mentioned by this skill against the base directory before \
             using them. Load referenced resources only as needed.\n</skill_resources>\n\n\
             <skill_instructions>\nSay hello, world.\n</skill_instructions>\n</skill_content>",
            base.display()
        )
    );
    assert!(
        texts[3].starts_with("Current runtime context."),
        "{}",
        texts[3]
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// A temporary directory of this test's own.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "karaka-cli-{name}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Two presets with their own personas and allow lists; `scout` also has a
/// skill of its own.
const PRESETS: &str = "
- id: coach
  name: Coach
  plugins:
    - name: persona
      config: { prefix: You are a patient coach. }
    - name: tool-policy
      config: { allow: [todo_write] }
- id: scout
  plugins:
    - name: '@deepseek-ai/dsh-persona'
      config: { prefix: You are a careful scout., suffix: Report back. }
    - name: '@karaka-ai/agent/tool-policy'
      config: { mode: native, allow: [working_directory, skill] }
    - name: skill-filesystem
      config: { includeDefaultRoots: false, customSkillDirs: [skills] }
";

/// `ask hi` with `extra` options, against a fresh DeepSeek stand-in, with
/// the presets in `dir`; gives the request body it sent.
fn ask_with_presets(dir: &Path, extra: &[&str]) -> Value {
    let (address, server) = serve_once(streamed(DEEPSEEK_HELLO));
    let mut args = vec!["ask"];
    args.extend_from_slice(extra);
    args.push("hi");
    let output = run_in(
        Some(dir),
        &args,
        &[
            ("KARAKA_PROVIDER", "deepseek".into()),
            ("DEEPSEEK_API_KEY", "test-key".into()),
            ("DEEPSEEK_BASE_URL", format!("http://{address}/anthropic")),
            (
                "KARAKA_PRESETS",
                dir.join("presets.yml").display().to_string(),
            ),
        ],
        "",
    );
    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let sent = server.join().expect("the server ran");
    let (_, body) = sent.split_once("\r\n\r\n").expect("headers, then a body");
    serde_json::from_str(body).unwrap()
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

// Each session runs its own preset: the default for one that names none,
// and `--preset`'s for one that picks another, each with its own persona,
// tool list and skills.
#[test]
fn each_session_gets_its_presets_persona_and_tools() {
    let dir = scratch("presets");
    std::fs::write(dir.join("presets.yml"), PRESETS).unwrap();
    let skill = dir.join("skills/demo");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo\ndescription: The scout's own skill.\n---\n\nScout around.\n",
    )
    .unwrap();

    let coach = ask_with_presets(&dir, &[]);
    assert_eq!(
        coach["system"],
        "You are an AI agent powered by Karaka.\n\nYou are a patient coach."
    );
    assert_eq!(tool_names(&coach), ["todo_write"]);
    assert!(!coach.to_string().contains("available_skills"));

    let scout = ask_with_presets(&dir, &["--preset", "scout"]);
    assert_eq!(
        scout["system"],
        "You are an AI agent powered by Karaka.\n\nYou are a careful scout.\n\nReport back."
    );
    assert_eq!(tool_names(&scout), ["skill", "working_directory"]);
    let catalog = scout["messages"][0]["content"][1]["text"].as_str().unwrap();
    assert!(
        catalog.contains("- `demo`: The scout's own skill."),
        "{catalog}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preset_mistakes_fail_before_any_call() {
    let dir = scratch("bad-presets");
    let presets = dir.join("presets.yml");
    std::fs::write(&presets, "- id: a\n  plugins: [{ name: shell }]\n").unwrap();
    let settings = |extra: Vec<(&'static str, String)>| {
        let mut settings = vec![
            ("KARAKA_PROVIDER", "deepseek".to_owned()),
            ("DEEPSEEK_API_KEY", "test-key".into()),
            // Nothing listens on port 1; a call that went out would fail
            // differently.
            ("DEEPSEEK_BASE_URL", "http://127.0.0.1:1".into()),
        ];
        settings.extend(extra);
        settings
    };

    let output = karaka(
        &["ask", "hi"],
        &settings(vec![("KARAKA_PRESETS", presets.display().to_string())]),
        "",
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        format!(
            "karaka: KARAKA_PRESETS ({}): preset 1 (\"a\"), plugin 1 (shell): unknown plugin \"shell\"; a preset may use persona, tool-policy, skill-filesystem and mcp-client\n",
            presets.display()
        )
    );

    std::fs::write(&presets, "- id: a\n").unwrap();
    let output = karaka(
        &["ask", "hi"],
        &settings(vec![
            ("KARAKA_PRESETS", presets.display().to_string()),
            ("KARAKA_PRESET", "b".into()),
        ]),
        "",
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output.stderr).contains("KARAKA_PRESET is \"b\", but"),
        "{}",
        text(&output.stderr)
    );

    let output = karaka(&["ask", "--preset", "a", "hi"], &settings(Vec::new()), "");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "karaka: --preset a needs KARAKA_PRESETS: without it there are no presets\n"
    );

    let output = karaka(
        &["ask", "--preset", "absent", "hi"],
        &settings(vec![("KARAKA_PRESETS", presets.display().to_string())]),
        "",
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "karaka: Unknown agent preset: absent\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
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
    assert!(
        first.contains(r#""You are an AI agent powered by Karaka.\n\nBe brief.""#),
        "{first}"
    );
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

/// The MCP fixture server: a 2025-era server needing only `sh` and `sed`,
/// with one tool, `add`.
fn mcp_fixture() -> String {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../karaka-mcp-client/tests/fixtures/server.sh"
    )
    .to_owned()
}

/// Anthropic's events for a call to `mcp__fixture__add` with 1 and 2.
const ANTHROPIC_MCP_ADD: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":12,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"mcp__fixture__add","input":{}}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1,\"b\":2}"}}"#,
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

// A preset's MCP server: its tool, under the server's namespace, and its
// instructions reach the request, and the model's call goes through to
// the server.
#[test]
fn a_presets_mcp_server_offers_its_tool_and_answers_the_call() {
    let dir = scratch("mcp-preset");
    let events = dir.join("events");
    std::fs::write(&events, "").unwrap();
    std::fs::write(
        dir.join("presets.yml"),
        format!(
            "- id: calculator\n  plugins:\n    - name: mcp-client\n      config:\n        transport: stdio\n        serverName: fixture\n        command: sh\n        args: [{:?}, {:?}]\n        reconnect: {{ enabled: false }}\n",
            mcp_fixture(),
            events.display().to_string()
        ),
    )
    .unwrap();
    let (address, server) = serve(vec![
        streamed(ANTHROPIC_MCP_ADD),
        streamed(&anthropic_text("It is 3.")),
    ]);

    let output = karaka(
        &["chat"],
        &[
            ("ANTHROPIC_API_KEY", "test-key".into()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
            (
                "KARAKA_PRESETS",
                dir.join("presets.yml").display().to_string(),
            ),
        ],
        "what is 1 + 2?\n",
    );

    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        text(&output.stdout),
        "[mcp__fixture__add gave: 3]\nIt is 3.\n"
    );
    let sent = server.join().expect("the server ran");
    let first: Value = serde_json::from_str(body(&sent[0])).unwrap();
    assert!(
        tool_names(&first).contains(&"mcp__fixture__add".to_owned()),
        "{first}"
    );
    // The server's instructions are a section of the system prompt.
    assert!(
        body(&sent[0]).contains(r#"### MCP server: fixture\n\nUse add."#),
        "{first}"
    );
    let second = body(&sent[1]);
    assert!(second.contains(r#""tool_use_id":"toolu_1""#), "{second}");
    // The fixture saw the call with the model's arguments.
    let log = std::fs::read_to_string(&events).unwrap();
    assert!(log.contains("tools/call"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

// A global server, an `mcp-client` row of `KARAKA_PLUGINS`, is offered to
// every session.
#[test]
fn a_global_mcp_server_is_offered_to_every_session() {
    let dir = scratch("mcp-global");
    let events = dir.join("events");
    std::fs::write(&events, "").unwrap();
    std::fs::write(
        dir.join("plugins.yml"),
        format!(
            "- name: mcp-client\n  config:\n    transport: stdio\n    serverName: fixture\n    command: sh\n    args: [{:?}, {:?}]\n",
            mcp_fixture(),
            events.display().to_string()
        ),
    )
    .unwrap();
    let (address, server) = serve_once(streamed(DEEPSEEK_HELLO));
    let output = karaka(
        &["ask", "hi"],
        &[
            ("KARAKA_PROVIDER", "deepseek".into()),
            ("DEEPSEEK_API_KEY", "test-key".into()),
            ("DEEPSEEK_BASE_URL", format!("http://{address}/anthropic")),
            (
                "KARAKA_PLUGINS",
                dir.join("plugins.yml").display().to_string(),
            ),
        ],
        "",
    );
    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let sent = server.join().expect("the server ran");
    let body: Value = serde_json::from_str(body(&sent)).unwrap();
    assert!(
        tool_names(&body).contains(&"mcp__fixture__add".to_owned()),
        "{body}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the application fixture was asked to call: each `tools/call`'s
/// params, and its `authorization` header.
type AppCalls = std::sync::Arc<std::sync::Mutex<Vec<(Value, Option<String>)>>>;

/// An application's MCP endpoint on a loopback port, speaking the 2025
/// era, with one tool, `lookup`. Gives its URL, and what it was called
/// with.
fn application_endpoint() -> (String, AppCalls) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let calls = AppCalls::default();
    let kept = AppCalls::clone(&calls);
    thread::spawn(move || {
        for socket in listener.incoming() {
            let Ok(mut socket) = socket else { return };
            let calls = AppCalls::clone(&kept);
            thread::spawn(move || {
                let request = read_request(&mut socket);
                let (head, body) = request
                    .split_once("\r\n\r\n")
                    .expect("headers, then a body");
                let respond = |socket: &mut TcpStream, status: &str, body: &str| {
                    let response =
                        http_response(status, &[("content-type", "application/json")], body);
                    let _ = socket.write_all(response.as_bytes());
                };
                if head.starts_with("GET") {
                    return respond(&mut socket, "405 Method Not Allowed", "");
                }
                let message: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                let id = message["id"].clone();
                let reply = |result: Value| {
                    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
                };
                match message["method"].as_str().unwrap_or_default() {
                    "server/discover" => respond(
                        &mut socket,
                        "200 OK",
                        &serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } }).to_string(),
                    ),
                    "initialize" => respond(
                        &mut socket,
                        "200 OK",
                        &reply(serde_json::json!({
                            "protocolVersion": "2025-06-18",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "application", "version": "1" },
                        })),
                    ),
                    "tools/list" => respond(
                        &mut socket,
                        "200 OK",
                        &reply(serde_json::json!({ "tools": [{
                            "name": "lookup",
                            "description": "Look a task up.",
                            "inputSchema": { "type": "object", "properties": {} },
                        }] })),
                    ),
                    "tools/call" => {
                        let authorization = head
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("authorization")
                                    .then(|| value.trim().to_owned())
                            });
                        calls
                            .lock()
                            .unwrap()
                            .push((message["params"].clone(), authorization));
                        respond(
                            &mut socket,
                            "200 OK",
                            &reply(serde_json::json!({ "content": [{ "type": "text", "text": "found it" }] })),
                        );
                    }
                    _ => respond(&mut socket, "202 Accepted", ""),
                }
            });
        }
    });
    (format!("http://{address}/mcp"), calls)
}

/// A plugins file with the application `app` and its endpoint at `url`,
/// whose tool credential is `APP_TOOLS`.
fn application_plugins(url: &str) -> String {
    format!(
        "- id: application-catalog\n  config: {{ servers: [app] }}\n\
         - name: '@karaka-ai/server-auth'\n  config:\n    applications: [{{ id: app, chatCredential: APP_CHAT, toolCredential: APP_TOOLS }}]\n\
         - id: app-tools\n  name: '@karaka-ai/mcp-application'\n  config:\n    applicationId: app\n    serverName: app\n    url: {url}\n    allow: [mcp__app__lookup]\n    reconnect: {{ enabled: false }}\n"
    )
}

/// A presets file whose one preset allows `todo_write` and the
/// application's tool.
const APPLICATION_PRESETS: &str = "- id: main\n  plugins:\n    - name: tool-policy\n      config: { allow: [todo_write, mcp__app__lookup] }\n";

/// The owner variables for `application`, tenant `tenant` and user `user`.
fn owned_by(application: &str) -> Vec<(&'static str, String)> {
    vec![
        ("KARAKA_APPLICATION_ID", application.into()),
        ("KARAKA_TENANT_ID", "tenant".into()),
        ("KARAKA_USER_ID", "user".into()),
    ]
}

/// Anthropic's events for a call to `mcp__app__lookup` with no arguments.
const ANTHROPIC_APP_LOOKUP: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":12,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"mcp__app__lookup","input":{}}}"#,
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

/// `ask hi` against a DeepSeek stand-in, with `env` besides; gives the
/// tools the request offered.
fn offered(env: &[(&'static str, String)]) -> Vec<String> {
    let (address, server) = serve_once(streamed(DEEPSEEK_HELLO));
    let mut settings = vec![
        ("KARAKA_PROVIDER", "deepseek".to_owned()),
        ("DEEPSEEK_API_KEY", "test-key".into()),
        ("DEEPSEEK_BASE_URL", format!("http://{address}/anthropic")),
    ];
    settings.extend_from_slice(env);
    let output = karaka(&["ask", "hi"], &settings, "");
    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let sent = server.join().expect("the server ran");
    let mut names = tool_names(&serde_json::from_str(body(&sent)).unwrap());
    names.sort();
    names
}

// An application's tool reaches the model only in a session the
// application owns, and its call carries the bearer the credential
// reference resolves to, with the owner and the session as
// `_meta.karaka`.
#[test]
fn an_application_tool_reaches_only_its_owners_session_and_carries_the_owner() {
    let dir = scratch("application");
    let (url, calls) = application_endpoint();
    let plugins = dir.join("plugins.yml");
    std::fs::write(&plugins, application_plugins(&url)).unwrap();
    std::fs::write(dir.join("presets.yml"), APPLICATION_PRESETS).unwrap();
    let base = vec![
        ("KARAKA_PLUGINS", plugins.display().to_string()),
        (
            "KARAKA_PRESETS",
            dir.join("presets.yml").display().to_string(),
        ),
        ("APP_TOOLS", "tool-secret".to_owned()),
    ];

    let (address, server) = serve(vec![
        streamed(ANTHROPIC_APP_LOOKUP),
        streamed(&anthropic_text("It is there.")),
    ]);
    let mut env = vec![
        ("ANTHROPIC_API_KEY", "test-key".to_owned()),
        ("ANTHROPIC_BASE_URL", format!("http://{address}")),
    ];
    env.extend(base.clone());
    env.extend(owned_by("app"));
    let output = karaka(&["ask", "find the task"], &env, "");
    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        text(&output.stdout).contains("It is there."),
        "{}",
        text(&output.stdout)
    );
    let sent = server.join().expect("the server ran");
    let first: Value = serde_json::from_str(body(&sent[0])).unwrap();
    assert!(
        tool_names(&first).contains(&"mcp__app__lookup".to_owned()),
        "{first}"
    );
    assert!(body(&sent[1]).contains("found it"), "{}", body(&sent[1]));
    assert_eq!(
        calls.lock().unwrap().clone(),
        [(
            serde_json::json!({
                "name": "lookup",
                "arguments": {},
                "_meta": { "karaka": {
                    "applicationId": "app", "tenantId": "tenant", "userId": "user", "chatId": "ask",
                } },
            }),
            Some("Bearer tool-secret".to_owned()),
        )]
    );

    // Another application's session, and one with no owner, aren't
    // offered it.
    let mut foreign = base.clone();
    foreign.extend(owned_by("other"));
    assert!(!offered(&foreign).contains(&"mcp__app__lookup".to_owned()));
    assert_eq!(offered(&base), ["todo_write"]);
    assert_eq!(calls.lock().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

// A preset's tool-policy allow list naming the application's tool keeps
// it, beside the global tools it names.
#[test]
fn a_presets_allow_list_keeps_its_application_tool() {
    let dir = scratch("application-preset");
    let (url, _) = application_endpoint();
    std::fs::write(dir.join("plugins.yml"), application_plugins(&url)).unwrap();
    std::fs::write(dir.join("presets.yml"), APPLICATION_PRESETS).unwrap();
    let mut env = vec![
        (
            "KARAKA_PLUGINS",
            dir.join("plugins.yml").display().to_string(),
        ),
        (
            "KARAKA_PRESETS",
            dir.join("presets.yml").display().to_string(),
        ),
        ("APP_TOOLS", "tool-secret".to_owned()),
    ];
    env.extend(owned_by("app"));
    assert_eq!(offered(&env), ["mcp__app__lookup", "todo_write"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_plugin_rows_fail_at_startup_saying_where() {
    let dir = scratch("bad-plugins");
    let plugins = dir.join("plugins.yml");
    let run = |env: Vec<(&'static str, String)>| {
        let mut settings = vec![
            ("KARAKA_PROVIDER", "deepseek".to_owned()),
            ("DEEPSEEK_API_KEY", "test-key".into()),
            ("DEEPSEEK_BASE_URL", "http://127.0.0.1:1".into()),
        ];
        settings.extend(env);
        let output = karaka(&["ask", "hi"], &settings, "");
        assert_eq!(output.status.code(), Some(1));
        text(&output.stderr)
    };
    let with_file = |contents: &str| {
        std::fs::write(&plugins, contents).unwrap();
        run(vec![("KARAKA_PLUGINS", plugins.display().to_string())])
    };
    let at = format!("KARAKA_PLUGINS ({})", plugins.display());
    assert_eq!(
        with_file("- transport: stdio\n  serverName: fixture\n"),
        format!(
            "karaka: {at}: plugin 1: unknown key \"transport\"; it may have id, name, disabled, config\n"
        )
    );
    assert_eq!(
        with_file("- name: mcp-client\n  config: { transport: stdio, serverName: fixture }\n"),
        format!("karaka: {at}: plugin 1 (mcp-client) needs a \"command\"\n")
    );
    assert_eq!(
        with_file(
            "- id: application-catalog\n  config: { servers: [app] }\n- name: mcp-application\n  config: { applicationId: app, serverName: app, url: 'http://127.0.0.1:1/mcp' }\n"
        ),
        format!(
            "karaka: {at}: plugin 2 (mcp-application): needs a server-auth row, for the endpoint's credentials\n"
        )
    );
    // An endpoint without presets would offer nothing.
    assert_eq!(
        with_file(&application_plugins("http://127.0.0.1:1/mcp")),
        format!(
            "karaka: {at}: the mcp-application row for \"app\" needs KARAKA_PRESETS: its tools reach only sessions whose preset's tool-policy allows them\n"
        )
    );
    // An endpoint that can't be reached fails the start, before any call.
    let presets = dir.join("presets.yml");
    std::fs::write(&presets, APPLICATION_PRESETS).unwrap();
    let stderr = run(vec![
        ("KARAKA_PLUGINS", plugins.display().to_string()),
        ("KARAKA_PRESETS", presets.display().to_string()),
    ]);
    assert!(
        stderr.starts_with("karaka: ") && stderr.contains("Application MCP app could not start"),
        "{stderr}"
    );
    // The old variable, and a partial owner.
    assert_eq!(
        run(vec![("KARAKA_MCP_SERVERS", plugins.display().to_string())]),
        "karaka: KARAKA_MCP_SERVERS is no longer read: list its servers as mcp-client rows in the file KARAKA_PLUGINS names\n"
    );
    assert_eq!(
        run(vec![("KARAKA_APPLICATION_ID", "app".into())]),
        "karaka: KARAKA_APPLICATION_ID, KARAKA_TENANT_ID and KARAKA_USER_ID name the sessions' owner together, so set all three or none; KARAKA_TENANT_ID and KARAKA_USER_ID are not set\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_chat_in_a_stored_session_carries_on_the_conversation() {
    // A fresh directory of its own: no test touches a real session store.
    let root = std::env::temp_dir().join(format!("karaka-cli-sessions-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let settings = |address: std::net::SocketAddr, session: &str| {
        vec![
            ("ANTHROPIC_API_KEY", "test-key".to_owned()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
            ("KARAKA_SESSIONS", root.display().to_string()),
            ("KARAKA_SESSION", session.to_owned()),
        ]
    };

    let (address, server) = serve(vec![streamed(&anthropic_text("Nice to meet you, Ada."))]);
    let output = karaka(&["chat"], &settings(address, "notes"), "my name is Ada\n");
    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(stderr, "");
    assert_eq!(text(&output.stdout), "Nice to meet you, Ada.\n");
    server.join().expect("the server ran");

    // The session is a file under the root, in a directory of its own.
    let logs: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .flat_map(|project| std::fs::read_dir(project.unwrap().path()).unwrap())
        .map(|session| session.unwrap().path().join("session.v4.jsonl.zstd"))
        .collect();
    assert_eq!(logs.len(), 1);
    assert!(logs[0].parent().unwrap().ends_with("notes"));
    // Compressed, the default: the header is the first frame's one line.
    let stored = std::fs::read(&logs[0]).unwrap();
    let frames = karaka_session_jsonl::frames::scan_frames(&stored, Some(1)).unwrap();
    let header =
        karaka_session_jsonl::frames::decompress_frame(&stored[frames.frames[0].clone()]).unwrap();
    let header = String::from_utf8(header).unwrap();
    assert!(
        header.starts_with(r#"{"type":"session","version":4,"id":"notes","#),
        "{header}"
    );

    // A second run in the same session carries the conversation on.
    let (address, server) = serve(vec![streamed(&anthropic_text("Your name is Ada."))]);
    let output = karaka(&["chat"], &settings(address, "notes"), "what is my name?\n");
    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(text(&output.stdout), "Your name is Ada.\n");
    let sent = server.join().expect("the server ran");
    let carried = body(&sent[0]);
    assert!(carried.contains("my name is Ada"), "{carried}");
    assert!(carried.contains("Nice to meet you, Ada."), "{carried}");
    assert!(carried.contains("what is my name?"), "{carried}");

    // Another session starts fresh.
    let (address, server) = serve(vec![streamed(&anthropic_text("I don't know."))]);
    let output = karaka(&["chat"], &settings(address, "other"), "what is my name?\n");
    assert!(output.status.success(), "{}", text(&output.stderr));
    let sent = server.join().expect("the server ran");
    assert!(!body(&sent[0]).contains("Ada"), "{}", body(&sent[0]));

    check_stored_streams(&root);
    std::fs::remove_dir_all(&root).unwrap();
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

/// `ask` goes through the loop, so a rate limit is retried: the server
/// refuses once, and the answer comes on the second request. The failed
/// attempt printed nothing, so there is nothing to take back and no note.
#[test]
fn ask_retries_a_rate_limit_and_answers() {
    let (address, server) = serve(vec![
        http_response(
            "429 Too Many Requests",
            &[("content-type", "application/json")],
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"Slow down."}}"#,
        ),
        streamed(ANTHROPIC_HELLO),
    ]);

    let output = karaka(
        &["ask", "say", "hello"],
        &[
            ("ANTHROPIC_API_KEY", "test-key".into()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
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
    let sent = server.join().expect("the server ran");
    assert_eq!(sent.len(), 2);
    assert_eq!(body(&sent[0]), body(&sent[1]));
}

/// Anthropic's events for "Hel", then an overloaded error mid-stream.
const ANTHROPIC_BREAKS_OFF: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_0","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":12,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
    "\n\n",
    "event: error\n",
    r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    "\n\n",
);

/// A retry after text was printed can't take the text back. The line is
/// ended, a note goes to stderr, and the retry's answer follows.
#[test]
fn ask_ends_the_line_and_says_so_when_it_retries_after_text() {
    let (address, server) = serve(vec![
        streamed(ANTHROPIC_BREAKS_OFF),
        streamed(ANTHROPIC_HELLO),
    ]);

    let output = karaka(
        &["ask", "say", "hello"],
        &[
            ("ANTHROPIC_API_KEY", "test-key".into()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
        ],
        "",
    );

    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(text(&output.stdout), "Hel\nHello, world\n");
    assert_eq!(
        stderr,
        "karaka: trying again\nstop: finished · in 12 (+100 cached) · out 34 tokens\n"
    );
    assert_eq!(server.join().expect("the server ran").len(), 2);
}

/// Anthropic's events for a call to `ask_user_question` with `input`.
fn anthropic_asks(input: &serde_json::Value) -> String {
    let partial = serde_json::to_string(&input.to_string()).unwrap();
    [
        r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":12,"output_tokens":1}}}"#.to_owned(),
        r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_ask","name":"ask_user_question","input":{}}}"#.to_owned(),
        format!(
            r#"event: content_block_delta
data: {{"type":"content_block_delta","index":0,"delta":{{"type":"input_json_delta","partial_json":{partial}}}}}"#
        ),
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}"#.to_owned(),
        r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":30}}"#.to_owned(),
        r#"event: message_stop
data: {"type":"message_stop"}"#.to_owned(),
    ]
    .map(|event| format!("{event}\n\n"))
    .concat()
}

/// A question with two options.
fn package_question() -> serde_json::Value {
    serde_json::json!({ "questions": [{
        "id": "pkg", "question": "Which package manager?", "header": "Setup",
        "options": [{ "label": "pnpm", "description": "Workspace default." }, { "label": "npm" }],
    }] })
}

/// The text of the tool result a request carries back for `toolu_ask`.
fn ask_result(request: &str) -> serde_json::Value {
    let body: serde_json::Value = serde_json::from_str(body(request)).unwrap();
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == "toolu_ask")
        .expect("the request carries the question's result")
}

/// The first text of a tool result block.
fn result_text(block: &serde_json::Value) -> String {
    match &block["content"] {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => blocks[0]["text"].as_str().unwrap().to_owned(),
        other => panic!("unexpected content {other}"),
    }
}

// While the model's question is open, the next line of stdin answers it;
// the line after is the chat's next message.
#[test]
fn chat_routes_the_next_line_to_an_open_question() {
    let (address, server) = serve(vec![
        streamed(&anthropic_asks(&package_question())),
        streamed(&anthropic_text("You picked npm.")),
        streamed(&anthropic_text("You asked which one you picked.")),
    ]);

    let output = karaka(
        &["chat"],
        &[
            ("ANTHROPIC_API_KEY", "test-key".into()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
        ],
        "set up the project\n2\nwhat did I pick?\n",
    );

    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    // The question is on stderr, so stdout stays the answers.
    assert_eq!(
        stderr,
        "\n[Setup] Which package manager?\n  1. pnpm - Workspace default.\n  2. npm\n\
         Pick a number, or type your own answer. Enter skips.\n> "
    );
    let stdout = text(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        [
            r#"[ask_user_question gave: {"answers":[{"id":"pkg","selected":["npm"]}]}]"#,
            "You picked npm.",
            "You asked which one you picked.",
        ],
        "{stdout}"
    );

    let sent = server.join().expect("the server ran");
    assert_eq!(
        result_text(&ask_result(&sent[1])),
        r#"{"answers":[{"id":"pkg","selected":["npm"]}]}"#
    );
    // The answer's line didn't become a message; the next line did.
    let third = body(&sent[2]);
    assert!(third.contains("what did I pick?"), "{third}");
    assert!(!third.contains(r#""text":"2""#), "{third}");
}

// `ask` reads stdin only for a question the model asks, so a piped line
// answers it.
#[test]
fn ask_answers_the_models_question_from_piped_stdin() {
    let (address, server) = serve(vec![
        streamed(&anthropic_asks(&package_question())),
        streamed(&anthropic_text("Using pnpm.")),
    ]);

    let output = karaka(
        &["ask", "set", "up", "the", "project"],
        &[
            ("ANTHROPIC_API_KEY", "test-key".into()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
        ],
        "1\n",
    );

    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("[Setup] Which package manager?\n"),
        "{stderr}"
    );
    // The printer starts each step's text on a line of its own.
    assert_eq!(text(&output.stdout), "\nUsing pnpm.\n");
    let sent = server.join().expect("the server ran");
    assert_eq!(
        result_text(&ask_result(&sent[1])),
        r#"{"answers":[{"id":"pkg","selected":["pnpm"]}]}"#
    );
}

// At the end of stdin, an open question is cancelled, not waited on: the
// model reads that it was aborted, and the run ends.
#[test]
fn the_end_of_stdin_cancels_an_open_question() {
    let (address, server) = serve(vec![
        streamed(&anthropic_asks(&package_question())),
        streamed(&anthropic_text("I'll pick pnpm myself.")),
    ]);

    let output = karaka(
        &["ask", "set", "up", "the", "project"],
        &[
            ("ANTHROPIC_API_KEY", "test-key".into()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
        ],
        "",
    );

    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(text(&output.stdout), "\nI'll pick pnpm myself.\n");
    let sent = server.join().expect("the server ran");
    let result = ask_result(&sent[1]);
    assert_eq!(result["is_error"], true);
    assert_eq!(
        result_text(&result),
        "Error: ask_user_question was aborted before the user answered"
    );
}

/// Anthropic's events for a call to `working_directory`, with no `cd`.
fn anthropic_reads_the_directory() -> String {
    anthropic_asks(&serde_json::json!({})).replace(
        "\"name\":\"ask_user_question\"",
        "\"name\":\"working_directory\"",
    )
}

/// The first text of the tool result a request carries back.
fn tool_result_text(request: &str) -> (bool, String) {
    let result = ask_result(request);
    (result["is_error"] == true, result_text(&result))
}

// `--ask-before` asks before each call to the tool it names, on stderr,
// and a `y` on stdin lets it run.
#[test]
fn ask_before_asks_and_y_allows_the_call() {
    let (address, server) = serve(vec![
        streamed(&anthropic_reads_the_directory()),
        streamed(&anthropic_text("You are in the project.")),
    ]);

    let output = karaka(
        &[
            "ask",
            "--ask-before",
            "working_directory",
            "where",
            "am",
            "I?",
        ],
        &[
            ("ANTHROPIC_API_KEY", "test-key".into()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
        ],
        "y\n",
    );

    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains(
            "\nAllow tool \"working_directory\"?\n  Reason: karaka was started with \
             --ask-before working_directory\nAllow it once? [y/N] "
        ),
        "{stderr}"
    );
    let sent = server.join().expect("the server ran");
    let (is_error, text) = tool_result_text(&sent[1]);
    assert!(!is_error, "{text}");
    assert_eq!(
        text,
        std::env::current_dir().unwrap().to_str().unwrap(),
        "the directory"
    );
}

// An empty line, or the end of stdin, withholds the approval, and the
// model reads why.
#[test]
fn ask_before_without_a_yes_denies_the_call() {
    for (stdin, reason) in [
        ("\n", "Error: the user rejected tool \"working_directory\""),
        (
            "",
            "Error: approval for tool \"working_directory\" was cancelled",
        ),
    ] {
        let (address, server) = serve(vec![
            streamed(&anthropic_reads_the_directory()),
            streamed(&anthropic_text("I couldn't look.")),
        ]);
        let output = karaka(
            &[
                "ask",
                "--ask-before",
                "working_directory",
                "where",
                "am",
                "I?",
            ],
            &[
                ("ANTHROPIC_API_KEY", "test-key".into()),
                ("ANTHROPIC_BASE_URL", format!("http://{address}")),
            ],
            stdin,
        );
        assert!(output.status.success(), "{}", text(&output.stderr));
        let sent = server.join().expect("the server ran");
        assert_eq!(tool_result_text(&sent[1]), (true, reason.to_owned()));
    }
}

// `--approval-policy never` rejects without asking anyone.
#[test]
fn the_never_policy_rejects_without_asking() {
    let (address, server) = serve(vec![
        streamed(&anthropic_reads_the_directory()),
        streamed(&anthropic_text("I couldn't look.")),
    ]);
    let output = karaka(
        &[
            "ask",
            "--approval-policy",
            "never",
            "--ask-before",
            "working_directory",
            "where",
            "am",
            "I?",
        ],
        &[
            ("ANTHROPIC_API_KEY", "test-key".into()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
        ],
        "y\n",
    );
    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("Allow tool"), "{stderr}");
    let sent = server.join().expect("the server ran");
    assert_eq!(
        tool_result_text(&sent[1]),
        (
            true,
            "Error: the user rejected tool \"working_directory\"".to_owned()
        )
    );
    // The model is told the policy, in the runtime context.
    assert!(
        body(&sent[0]).contains("Approval prompts are disabled in this session"),
        "{}",
        body(&sent[0])
    );
}

// In chat, the line after an approval prompt answers it; the next one is
// the next message.
#[test]
fn chat_routes_the_next_line_to_an_open_approval() {
    let (address, server) = serve(vec![
        streamed(&anthropic_reads_the_directory()),
        streamed(&anthropic_text("I couldn't look.")),
        streamed(&anthropic_text("Fine.")),
    ]);
    let output = karaka(
        &["chat", "--ask-before", "working_directory"],
        &[
            ("ANTHROPIC_API_KEY", "test-key".into()),
            ("ANTHROPIC_BASE_URL", format!("http://{address}")),
        ],
        "where am I?\nn\nnever mind\n",
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    let sent = server.join().expect("the server ran");
    assert_eq!(
        tool_result_text(&sent[1]),
        (
            true,
            "Error: the user rejected tool \"working_directory\"".to_owned()
        )
    );
    let third = body(&sent[2]);
    assert!(third.contains("never mind"), "{third}");
    assert!(!third.contains(r#""text":"n""#), "{third}");
}

#[test]
fn a_bad_approval_policy_prints_why_and_the_usage() {
    let output = karaka(&["chat", "--approval-policy", "sometimes"], &[], "");
    assert_eq!(output.status.code(), Some(2));
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("karaka: approval policy must be one of \"ask\" or \"never\"\nUsage:"),
        "{stderr}"
    );
}
