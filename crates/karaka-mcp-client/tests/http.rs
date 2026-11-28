//! Streamable HTTP against a small in-process server: a 2025-era handshake
//! with a session id, a JSON reply and an SSE reply, the GET stream's 405,
//! and the headers each request carries. (dsh: the SDK's
//! `StreamableHTTPClientTransport`, which dsh's `http-fixture.ts` serves.)

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use karaka_mcp_client::{Client, ClientOptions, Era, HttpParams, HttpTransport, RequestOptions};
use serde_json::{Value, json};

/// One request as the server read it.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    head: String,
    body: String,
}

fn read_request(socket: &mut TcpStream) -> Seen {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]).to_string();
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().to_owned())
                })
                .map_or(0, |value| value.parse().unwrap());
            while bytes.len() < end + 4 + length {
                let read = socket.read(&mut chunk).unwrap();
                bytes.extend_from_slice(&chunk[..read]);
            }
            let body = String::from_utf8_lossy(&bytes[end + 4..end + 4 + length]).to_string();
            let method = head.split(' ').next().unwrap_or_default().to_owned();
            return Seen {
                method,
                head: head.to_lowercase(),
                body,
            };
        }
        let read = socket.read(&mut chunk).unwrap();
        assert!(read > 0);
        bytes.extend_from_slice(&chunk[..read]);
    }
}

fn respond(socket: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &str) {
    let mut response = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    ));
    socket.write_all(response.as_bytes()).unwrap();
}

#[tokio::test]
async fn a_legacy_http_server_round_trips() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let kept = Arc::clone(&seen);
    thread::spawn(move || {
        for socket in listener.incoming() {
            let mut socket = socket.unwrap();
            let request = read_request(&mut socket);
            kept.lock().unwrap().push(request.clone());
            if request.method == "GET" {
                respond(&mut socket, "405 Method Not Allowed", &[], "");
                continue;
            }
            let message: Value = serde_json::from_str(&request.body).unwrap();
            let id = message["id"].clone();
            let json_headers = [("content-type", "application/json")];
            match message["method"].as_str().unwrap_or_default() {
                "server/discover" => respond(
                    &mut socket,
                    "200 OK",
                    &json_headers,
                    &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } }).to_string(),
                ),
                "initialize" => respond(
                    &mut socket,
                    "200 OK",
                    &[("content-type", "application/json"), ("mcp-session-id", "session-1")],
                    &json!({ "jsonrpc": "2.0", "id": id, "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "http-fixture", "version": "1" },
                    } }).to_string(),
                ),
                "tools/list" => {
                    let event = json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [{ "name": "echo" }] } });
                    respond(
                        &mut socket,
                        "200 OK",
                        &[("content-type", "text/event-stream")],
                        &format!("event: message\nid: 1\ndata: {event}\n\n"),
                    );
                }
                _ => respond(&mut socket, "202 Accepted", &[], ""),
            }
        }
    });

    let transport = HttpTransport::new(HttpParams {
        url: format!("http://{address}/mcp"),
        headers: vec![("x-app".into(), "karaka".into())],
    });
    let client = Client::new(Box::new(transport), ClientOptions::default());
    client.connect().await.unwrap();
    assert_eq!(client.era(), Some(Era::Legacy("2025-06-18".into())));
    let tools = client
        .list_all_tools(RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(tools, [json!({ "name": "echo" })]);

    let seen = seen.lock().unwrap().clone();
    let posts: Vec<&Seen> = seen
        .iter()
        .filter(|request| request.method == "POST")
        .collect();
    // The probe carries the modern revision and its method, from its
    // envelope.
    assert!(
        posts[0].head.contains("mcp-protocol-version: 2026-07-28"),
        "{}",
        posts[0].head
    );
    assert!(posts[0].head.contains("mcp-method: server/discover"));
    assert!(
        posts[0]
            .head
            .contains("accept: application/json, text/event-stream")
    );
    assert!(posts[0].head.contains("x-app: karaka"));
    // `initialize` carries no session; everything after it the server's,
    // with the negotiated revision.
    assert!(!posts[1].head.contains("mcp-session-id"));
    for later in &posts[2..] {
        assert!(
            later.head.contains("mcp-session-id: session-1"),
            "{}",
            later.head
        );
        assert!(
            later.head.contains("mcp-protocol-version: 2025-06-18"),
            "{}",
            later.head
        );
    }
    // The GET stream opened after `notifications/initialized`, and its 405
    // ended it quietly.
    assert!(seen.iter().any(|request| request.method == "GET"
        && request.head.contains("accept: text/event-stream")
        && request.head.contains("mcp-session-id: session-1")));
    client.close().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), client.closed())
        .await
        .expect("the close is reported");
}

// A non-2xx answer is the SDK's error, with its status kept for the probe.
#[tokio::test]
async fn an_error_status_is_the_sdks_message() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for socket in listener.incoming() {
            let mut socket = socket.unwrap();
            let _ = read_request(&mut socket);
            respond(
                &mut socket,
                "503 Service Unavailable",
                &[],
                "temporary failure",
            );
        }
    });
    let transport = HttpTransport::new(HttpParams {
        url: format!("http://{address}/mcp"),
        headers: Vec::new(),
    });
    let error = Client::new(Box::new(transport), ClientOptions::default())
        .connect()
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "Version negotiation failed: the server answered the probe with HTTP 503"
    );
}
