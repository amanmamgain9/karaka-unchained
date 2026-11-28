//! A real stdio round trip, against `fixtures/server.sh`. (dsh:
//! `negotiation-lifecycle.spec.ts`'s stdio cases, and the e2e fixture
//! server's round trip.)

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Recorder, wait_for};
use karaka_mcp_client::{
    ConnectionConfig, Level, ReconnectPolicy, StdioParams, StdioTransport, Transport,
    TransportConfig, child_env, start_connection, transport_factory,
};
use serde_json::json;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "karaka-mcp-stdio-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/server.sh").to_owned()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The fixture's events: `(event, pid, rest)`.
fn events(path: &PathBuf) -> Vec<(String, i32, String)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| {
            let mut parts = line.splitn(3, ' ');
            let event = parts.next().unwrap_or_default().to_owned();
            let pid = parts.next().unwrap_or_default().parse().unwrap_or(0);
            (event, pid, parts.next().unwrap_or_default().to_owned())
        })
        .collect()
}

// "reaps the probe before starting the serving process": the probe twin is
// gone before the session's process starts, the session's process never sees
// `server/discover`, tools and calls go through, and disposal ends both.
#[tokio::test]
async fn a_stdio_server_round_trips_and_is_reaped() {
    let dir = TempDir::new();
    let events_path = dir.0.join("events");
    std::fs::write(&events_path, "").unwrap();
    let recorder = Arc::new(Recorder::default());
    let handle = start_connection(
        ConnectionConfig::new("fixture"),
        ReconnectPolicy {
            enabled: false,
            ..karaka_mcp_client::RECONNECT_DEFAULTS
        },
        transport_factory(TransportConfig::Stdio(StdioParams {
            command: "sh".into(),
            args: vec![fixture(), events_path.to_string_lossy().into_owned()],
            ..StdioParams::default()
        })),
        Arc::clone(&recorder) as _,
    );
    handle.ready().await.unwrap();
    assert_eq!(recorder.published(), ["add"]);
    assert_eq!(handle.instructions(), "### MCP server: fixture\n\nUse add.");
    let result = handle
        .client()
        .unwrap()
        .call_tool("add", json!({ "a": 1, "b": 2 }), Default::default())
        .await
        .unwrap();
    assert_eq!(result["content"][0]["text"], "3");

    let starts: Vec<_> = events(&events_path)
        .into_iter()
        .filter(|(event, _, _)| event == "start")
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1].2, "false", "the probe was reaped first");
    let (probe, session) = (starts[0].1, starts[1].1);
    let methods = |pid: i32| -> Vec<String> {
        events(&events_path)
            .into_iter()
            .filter(|(event, at, _)| *at == pid && event != "start" && event != "exit")
            .map(|(event, _, _)| event)
            .collect()
    };
    assert_eq!(methods(probe), ["server/discover"]);
    assert_eq!(methods(session), ["initialize", "tools/list", "tools/call"]);

    handle.dispose().await;
    wait_for(|| !alive(probe) && !alive(session)).await;
    assert!(recorder.lines(Level::Error).is_empty());
}

// "retries failed stdio probes when no child could be spawned"
#[tokio::test]
async fn a_command_that_wont_spawn_is_retried_to_the_cap() {
    let made = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&made);
    let factory = Arc::new(move || -> Box<dyn Transport> {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::new(StdioTransport::new(StdioParams {
            command: "/nonexistent/karaka-mcp-missing-command".into(),
            ..StdioParams::default()
        }))
    });
    let recorder = Arc::new(Recorder::default());
    let handle = start_connection(
        ConnectionConfig::new("fixture"),
        ReconnectPolicy {
            enabled: true,
            initial_delay_ms: 1.0,
            max_delay_ms: 1.0,
            max_attempts: 3,
        },
        factory,
        Arc::clone(&recorder) as _,
    );
    let error = handle.ready().await.unwrap_err();
    assert!(
        error
            .message
            .starts_with("spawn /nonexistent/karaka-mcp-missing-command "),
        "{error}"
    );
    wait_for(|| recorder.has(Level::Error, "giving up after 3")).await;
    assert_eq!(made.load(Ordering::SeqCst), 4);
    assert_eq!(
        recorder
            .lines(Level::Warn)
            .iter()
            .filter(|line| line.contains("connection failed; retrying"))
            .count(),
        3
    );
    assert_eq!(recorder.lines(Level::Error).len(), 1);
}

// The server's environment: the spec's `env` over this process's, with
// nothing that looks like a credential, and no `DSH_*`.
#[test]
fn the_child_environment_is_scrubbed() {
    let mut extra = BTreeMap::new();
    extra.insert("FIXTURE_MODE".to_owned(), "on".to_owned());
    let env = child_env(&extra);
    assert_eq!(env.get("FIXTURE_MODE").map(String::as_str), Some("on"));
    for name in env.keys().filter(|name| *name != "FIXTURE_MODE") {
        let upper = name.to_uppercase();
        assert!(
            !["KEY", "PASSWORD", "SECRET", "TOKEN"]
                .iter()
                .any(|word| upper.contains(word)),
            "{name}"
        );
        assert!(!upper.starts_with("DSH_"), "{name}");
    }
}
