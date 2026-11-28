//! One connection: negotiation, the handshake, requests, and what the
//! server sends unasked, against a scripted server. (dsh:
//! `mcp-client/tests/protocol.spec.ts`, for what step A has: its resource
//! and tool-definition halves come with the tools; and the SDK's behaviour
//! dsh relies on.)

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Knobs, Reply, Scripted, ScriptedTransport, http_error, legacy_server, wait_for};
use karaka_fs::AbortController;
use karaka_mcp_client::{
    Client, ClientOptions, Era, ErrorKind, McpError, RequestOptions, TransportKind,
};
use serde_json::{Value, json};

fn client(scripted: &Arc<Scripted>, options: ClientOptions) -> Client {
    Client::new(Box::new(ScriptedTransport(Arc::clone(scripted))), options)
}

fn counter() -> (Arc<AtomicUsize>, ClientOptions) {
    let changes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&changes);
    let options = ClientOptions {
        on_tools_changed: Some(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })),
        ..ClientOptions::default()
    };
    (changes, options)
}

// A 2025-era server: the probe's error falls back to `initialize`, with the
// SDK's exact messages.
#[tokio::test]
async fn a_legacy_server_gets_the_initialize_handshake() {
    let scripted = Scripted::new(legacy_server(&["add"]), Knobs::default());
    let client = client(&scripted, ClientOptions::default());
    client.connect().await.unwrap();

    assert_eq!(client.era(), Some(Era::Legacy("2025-11-25".into())));
    let sent = scripted.sent();
    assert_eq!(
        sent[0],
        json!({
            "jsonrpc": "2.0", "id": "server-discover-probe-1", "method": "server/discover",
            "params": { "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": { "name": "dsh-mcp-client", "version": "0.0.1" },
                "io.modelcontextprotocol/clientCapabilities": {},
            } },
        })
    );
    assert_eq!(
        sent[1],
        json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": { "name": "dsh-mcp-client", "version": "0.0.1" },
            },
        })
    );
    assert_eq!(
        sent[2],
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    );
    assert_eq!(
        client.server_info(),
        json!({ "name": "fixture", "version": "1" })
    );
    let tools = client
        .list_all_tools(RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(tools[0]["name"], "add");
    // A 2025-era request carries no envelope.
    assert_eq!(
        scripted.sent()[3],
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })
    );
}

#[tokio::test]
async fn an_unsupported_initialize_version_fails_the_connection() {
    let scripted = Scripted::new(
        Arc::new(
            |message: &Value| match message["method"].as_str().unwrap_or_default() {
                "initialize" => Reply::Result(json!({
                    "protocolVersion": "1999-01-01", "capabilities": {}, "serverInfo": {},
                })),
                _ => Reply::Error(-32601, "Method not found"),
            },
        ),
        Knobs::default(),
    );
    let error = client(&scripted, ClientOptions::default())
        .connect()
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "Server's protocol version is not supported: 1999-01-01"
    );
}

// A `server/discover` offering 2026-07-28 is a modern verdict: no
// `initialize`, an envelope on every request, and changes through
// `subscriptions/listen`.
#[tokio::test]
async fn a_modern_server_is_used_in_its_era() {
    let scripted = Scripted::new(
        Arc::new(
            |message: &Value| match message["method"].as_str().unwrap_or_default() {
                "server/discover" => Reply::Result(json!({
                    "supportedVersions": ["2026-07-28"],
                    "capabilities": { "tools": { "listChanged": true } },
                    "instructions": "Use the modern tools.",
                    "_meta": { "io.modelcontextprotocol/serverInfo": { "name": "modern", "version": "2" } },
                })),
                "tools/list" => {
                    Reply::Result(json!({ "tools": [], "ttlMs": 0, "cacheScope": "private" }))
                }
                _ => Reply::Silent,
            },
        ),
        Knobs {
            kind: TransportKind::Http,
            ..Knobs::default()
        },
    );
    let (changes, options) = counter();
    let errors = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&errors);
    let client = client(
        &scripted,
        ClientOptions {
            on_error: Some(Arc::new(move |error| kept.lock().unwrap().push(error))),
            ..options
        },
    );
    client.connect().await.unwrap();

    assert_eq!(client.era(), Some(Era::Modern));
    assert_eq!(
        client.instructions().as_deref(),
        Some("Use the modern tools.")
    );
    assert_eq!(
        client.server_info(),
        json!({ "name": "modern", "version": "2" })
    );
    assert!(!scripted.methods().contains(&"initialize".to_owned()));
    wait_for(|| {
        scripted
            .methods()
            .contains(&"subscriptions/listen".to_owned())
    })
    .await;
    let listen = scripted
        .sent()
        .into_iter()
        .find(|message| message["method"] == "subscriptions/listen")
        .unwrap();
    assert_eq!(listen["id"], "listen:0");
    assert_eq!(
        listen["params"]["notifications"],
        json!({ "toolsListChanged": true })
    );
    scripted.push(json!({
        "jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged",
        "params": { "_meta": { "io.modelcontextprotocol/subscriptionId": "listen:0" } },
    }));

    client
        .list_tools(None, RequestOptions::default())
        .await
        .unwrap();
    let list = scripted
        .sent()
        .into_iter()
        .find(|message| message["method"] == "tools/list")
        .unwrap();
    assert_eq!(
        list["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
        "2026-07-28"
    );
    scripted.push(json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }));
    wait_for(|| changes.load(Ordering::SeqCst) == 1).await;
    // No ping in this era.
    let error = client
        .request("ping", None, RequestOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "Method 'ping' is not supported by the negotiated protocol version (wire era 2026-07-28)"
    );
    assert!(errors.lock().unwrap().is_empty());
}

// stdio probes on a disposable twin, so the server's own process never sees
// `server/discover`; the twin is closed whatever it found.
#[tokio::test]
async fn stdio_probes_on_a_twin_and_reaps_it() {
    let scripted = Scripted::new(
        legacy_server(&[]),
        Knobs {
            sibling: true,
            ..Knobs::default()
        },
    );
    let client = client(&scripted, ClientOptions::default());
    client.connect().await.unwrap();
    let twin = scripted.twin.lock().unwrap().clone().unwrap();
    assert_eq!(twin.methods(), ["server/discover"]);
    assert_eq!(twin.closes.load(Ordering::SeqCst), 1);
    assert_eq!(
        scripted.methods(),
        ["initialize", "notifications/initialized"]
    );
}

// On stdio a probe that times out, or a server that closes on it, is a
// legacy server; on HTTP each fails the connection.
#[tokio::test]
async fn a_silent_or_closing_probe_means_legacy_on_stdio_only() {
    let silent = |closes: bool| -> common::Script {
        Arc::new(
            move |message: &Value| match message["method"].as_str().unwrap_or_default() {
                "server/discover" if closes => Reply::Fail(McpError::transport("never")),
                "server/discover" => Reply::Silent,
                "initialize" => Reply::Result(json!({
                    "protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {},
                })),
                _ => Reply::Silent,
            },
        )
    };
    let options = ClientOptions {
        probe_timeout: Some(Duration::from_millis(50)),
        ..ClientOptions::default()
    };
    let stdio = Scripted::new(silent(false), Knobs::default());
    let client_on_stdio = client(&stdio, options.clone());
    client_on_stdio.connect().await.unwrap();
    assert_eq!(
        client_on_stdio.era(),
        Some(Era::Legacy("2025-06-18".into()))
    );

    let http = Scripted::new(
        silent(false),
        Knobs {
            kind: TransportKind::Http,
            ..Knobs::default()
        },
    );
    let error = client(&http, options.clone()).connect().await.unwrap_err();
    assert_eq!(
        error.message,
        "Version negotiation probe timed out after 50ms"
    );
    assert_eq!(error.kind, ErrorKind::RequestTimeout);
    assert_eq!(http.closes.load(Ordering::SeqCst), 1);

    // The server closing on the probe.
    let closing = Scripted::new(
        Arc::new(|_: &Value| Reply::Silent),
        Knobs {
            kind: TransportKind::Http,
            ..Knobs::default()
        },
    );
    let closing_client = client(&closing, options);
    let connecting = tokio::spawn({
        let client = closing_client.clone();
        async move { client.connect().await }
    });
    wait_for(|| !closing.sent().is_empty()).await;
    closing.drop_connection();
    assert_eq!(
        connecting.await.unwrap().unwrap_err().message,
        "Connection closed during the version negotiation probe"
    );
}

// An HTTP status answers the probe: 401, 403 and 5xx fail the connection,
// another 4xx is a legacy server; a network error fails it.
#[tokio::test]
async fn http_statuses_classify_the_probe() {
    for (status, outcome) in [
        (
            401,
            Some("Version negotiation failed: the server requires authorization (HTTP 401)"),
        ),
        (
            403,
            Some("Version negotiation failed: the server denied access (HTTP 403)"),
        ),
        (
            503,
            Some("Version negotiation failed: the server answered the probe with HTTP 503"),
        ),
        (404, None),
    ] {
        let scripted = Scripted::new(
            Arc::new(
                move |message: &Value| match message["method"].as_str().unwrap_or_default() {
                    "server/discover" => Reply::Fail(http_error(status)),
                    "initialize" => Reply::Result(json!({
                        "protocolVersion": "2025-03-26", "capabilities": {}, "serverInfo": {},
                    })),
                    _ => Reply::Silent,
                },
            ),
            Knobs {
                kind: TransportKind::Http,
                ..Knobs::default()
            },
        );
        let connected = client(&scripted, ClientOptions::default()).connect().await;
        match outcome {
            Some(message) => assert_eq!(connected.unwrap_err().message, message),
            None => assert!(connected.is_ok(), "{status}"),
        }
    }
    let network = Scripted::new(
        Arc::new(|_: &Value| Reply::Fail(McpError::transport("connection refused"))),
        Knobs {
            kind: TransportKind::Http,
            ..Knobs::default()
        },
    );
    assert_eq!(
        client(&network, ClientOptions::default())
            .connect()
            .await
            .unwrap_err()
            .message,
        "Version negotiation probe failed: connection refused"
    );
}

#[tokio::test]
async fn tools_are_listed_page_by_page() {
    let scripted = Scripted::new(
        Arc::new(
            |message: &Value| match message["method"].as_str().unwrap_or_default() {
                "initialize" => Reply::Result(json!({
                    "protocolVersion": "2025-11-25", "capabilities": { "tools": {} }, "serverInfo": {},
                })),
                "tools/list" => match message["params"]["cursor"].as_str() {
                    None => {
                        Reply::Result(json!({ "tools": [{ "name": "a" }], "nextCursor": "page-2" }))
                    }
                    Some(_) => Reply::Result(json!({ "tools": [{ "name": "b" }] })),
                },
                _ => Reply::Error(-32601, "Method not found"),
            },
        ),
        Knobs::default(),
    );
    let client = client(&scripted, ClientOptions::default());
    client.connect().await.unwrap();
    let tools = client
        .list_all_tools(RequestOptions::default())
        .await
        .unwrap();
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a", "b"]);
    let page = client
        .list_tools(Some("page-2"), RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(page.tools, [json!({ "name": "b" })]);
    assert_eq!(page.next_cursor, None);
    let lists: Vec<Value> = scripted
        .sent()
        .into_iter()
        .filter(|message| message["method"] == "tools/list")
        .map(|message| message.get("params").cloned().unwrap_or(Value::Null))
        .collect();
    assert_eq!(
        lists,
        [
            Value::Null,
            json!({ "cursor": "page-2" }),
            json!({ "cursor": "page-2" })
        ]
    );
}

// A server's error is passed on as it wrote it; a timeout and a cancel each
// tell the server the request was given up, in the SDK's words.
#[tokio::test]
async fn errors_timeouts_and_cancels() {
    let scripted = Scripted::new(
        Arc::new(
            |message: &Value| match message["method"].as_str().unwrap_or_default() {
                "server/discover" => Reply::Error(-32601, "Method not found"),
                "initialize" => Reply::Result(json!({
                    "protocolVersion": "2025-11-25", "capabilities": {}, "serverInfo": {},
                })),
                "tools/call" if message["params"]["name"] == "broken" => {
                    Reply::Error(-32602, "Bad arguments")
                }
                _ => Reply::Silent,
            },
        ),
        Knobs::default(),
    );
    let client = client(&scripted, ClientOptions::default());
    client.connect().await.unwrap();

    let error = client
        .call_tool("broken", json!({}), RequestOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.message, "Bad arguments");
    assert_eq!(error.code(), Some(-32602));

    let error = client
        .call_tool(
            "slow",
            json!({}),
            RequestOptions {
                timeout: Some(Duration::from_millis(20)),
                cancel: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.message, "Request timed out");
    assert_eq!(error.kind, ErrorKind::RequestTimeout);
    let cancelled = scripted.sent().last().cloned().unwrap();
    assert_eq!(cancelled["method"], "notifications/cancelled");
    assert_eq!(cancelled["params"]["reason"], "SdkError: Request timed out");

    let controller = AbortController::new();
    let calling = tokio::spawn({
        let client = client.clone();
        let cancel = controller.signal();
        async move {
            client
                .call_tool(
                    "slow",
                    json!({}),
                    RequestOptions {
                        timeout: None,
                        cancel: Some(cancel),
                    },
                )
                .await
        }
    });
    wait_for(|| scripted.sent().len() >= 7).await;
    controller.abort();
    let error = calling.await.unwrap().unwrap_err();
    assert_eq!(error.message, "AbortError: This operation was aborted");
    let cancelled = scripted.sent().last().cloned().unwrap();
    assert_eq!(cancelled["params"]["requestId"], 3);
    assert_eq!(
        cancelled["params"]["reason"],
        "AbortError: This operation was aborted"
    );
}

// The client answers the server's `ping`, and `Method not found` to the
// rest; a close fails what is pending; a request after it isn't sent.
#[tokio::test]
async fn ping_unknown_requests_and_close() {
    let scripted = Scripted::new(legacy_server(&[]), Knobs::default());
    let client = client(&scripted, ClientOptions::default());
    client.connect().await.unwrap();
    scripted.push(json!({ "jsonrpc": "2.0", "id": 9, "method": "ping" }));
    scripted.push(json!({ "jsonrpc": "2.0", "id": 10, "method": "sampling/createMessage" }));
    wait_for(|| scripted.sent().len() >= 5).await;
    let sent = scripted.sent();
    assert_eq!(sent[3], json!({ "jsonrpc": "2.0", "id": 9, "result": {} }));
    assert_eq!(
        sent[4],
        json!({ "jsonrpc": "2.0", "id": 10, "error": { "code": -32601, "message": "Method not found" } })
    );

    scripted.set_script(Arc::new(|_: &Value| Reply::Silent));
    let pending = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call_tool("x", json!({}), RequestOptions::default())
                .await
        }
    });
    wait_for(|| scripted.sent().len() >= 6).await;
    scripted.drop_connection();
    assert_eq!(
        pending.await.unwrap().unwrap_err().message,
        "Connection closed"
    );
    assert!(client.is_closed());
    assert_eq!(
        client
            .call_tool("x", json!({}), RequestOptions::default())
            .await
            .unwrap_err()
            .message,
        "Not connected"
    );
}

// `list_changed` is heard only when the server declares `tools.listChanged`.
#[tokio::test]
async fn tool_changes_need_the_servers_capability() {
    let scripted = Scripted::new(
        Arc::new(
            |message: &Value| match message["method"].as_str().unwrap_or_default() {
                "initialize" => Reply::Result(json!({
                    "protocolVersion": "2025-11-25", "capabilities": { "tools": {} }, "serverInfo": {},
                })),
                _ => Reply::Error(-32601, "Method not found"),
            },
        ),
        Knobs::default(),
    );
    let (changes, options) = counter();
    let client = client(&scripted, options);
    client.connect().await.unwrap();
    scripted.push(json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(changes.load(Ordering::SeqCst), 0);

    let declares = Scripted::new(legacy_server(&[]), Knobs::default());
    let (changes, options) = counter();
    let client = self::client(&declares, options);
    client.connect().await.unwrap();
    declares.push(json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }));
    wait_for(|| changes.load(Ordering::SeqCst) == 1).await;
}
