//! The connection supervisor. Ported from dsh's
//! `mcp-client/tests/reconnect.spec.ts`, its "reconnect supervisor" and
//! "resolveReconnectPolicy" cases, against scripted servers where dsh mocks
//! the SDK's `Client`. The recording observer stands in for dsh's tool
//! registry: its published names are the server's registered tools.
//!
//! Not ported: "rejects resource reads while a replacement connection is
//! still negotiating" is `client()`'s disconnected error here, which
//! `a_replacement_still_negotiating_is_disconnected` checks; the cases
//! naming `apply` (load-time failure, the plugin's dispose) wait on the
//! plugin, step B; and the cases about the SDK's own close races (a close
//! during a resolving connect, a transport attaching after disposal),
//! which a client that owns its read loop covers in
//! `a_close_during_the_handshake_registers_nothing`.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{Factory, Knobs, Recorder, Reply, failing_server, legacy_server, wait_for};
use karaka_mcp_client::{
    ConnectionConfig, ConnectionHandle, Level, RECONNECT_DEFAULTS, ReconnectPolicy,
    resolve_reconnect_policy, start_connection,
};
use serde_json::{Value, json};

fn policy(initial: f64, max: f64, attempts: u64) -> ReconnectPolicy {
    ReconnectPolicy {
        enabled: true,
        initial_delay_ms: initial,
        max_delay_ms: max,
        max_attempts: attempts,
    }
}

fn start(factory: &Factory, policy: ReconnectPolicy) -> (ConnectionHandle, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    let handle = start_connection(
        ConnectionConfig::new("srv"),
        policy,
        factory.factory(),
        Arc::clone(&recorder) as _,
    );
    (handle, recorder)
}

// "reconnects after a transport close, re-syncs tools through the new
// generation, and serves calls"
#[tokio::test]
async fn a_lost_connection_reconnects_and_resyncs() {
    let factory = Factory::new(legacy_server(&["remote"]), Knobs::default());
    let (handle, recorder) = start(&factory, policy(5.0, 40.0, 5));
    handle.ready().await.unwrap();
    assert_eq!(recorder.published(), ["remote"]);

    factory.set_script(legacy_server(&["revived"]));
    factory.made()[0].drop_connection();
    wait_for(|| recorder.published() == ["revived"]).await;
    assert_eq!(factory.count(), 2);
    assert!(recorder.has(
        Level::Warn,
        "mcp-client(srv): connection lost; reconnecting in 5ms (attempt 1/5)"
    ));
    assert!(recorder.has(
        Level::Info,
        "mcp-client(srv): reconnected and re-synced tools (attempt 1/5)"
    ));
    // Calls go through the new generation.
    let client = handle.client().unwrap();
    client
        .call_tool("revived", json!({}), Default::default())
        .await
        .unwrap();
    // A late close from the replaced generation schedules nothing.
    factory.made()[0].drop_connection();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(factory.count(), 2);
    handle.dispose().await;
}

// "stops at the failure cap, unregisters the tools, and reports final
// failure"
#[tokio::test]
async fn the_failure_cap_clears_the_tools_and_stops() {
    let factory = Factory::new(legacy_server(&["remote"]), Knobs::default());
    let (handle, recorder) = start(&factory, policy(2.0, 8.0, 2));
    handle.ready().await.unwrap();

    factory.set_script(failing_server("server gone"));
    factory.made()[0].drop_connection();
    wait_for(|| {
        recorder.has(
            Level::Error,
            "giving up after 2 consecutive failed reconnect attempts",
        )
    })
    .await;
    assert!(recorder.has(
        Level::Error,
        "mcp-client(srv): giving up after 2 consecutive failed reconnect attempts — tools unregistered; reload the plugin or restart the Host to reconnect"
    ));
    wait_for(|| recorder.published().is_empty()).await;
    // The first connection and exactly two more.
    assert_eq!(factory.count(), 3);
    assert!(recorder.has(
        Level::Warn,
        "connection attempt failed: ProtocolError: server gone"
    ));
    assert!(recorder.has(
        Level::Warn,
        "connection failed; retrying in 4ms (attempt 2/2)"
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(factory.count(), 3);
    handle.dispose().await;
}

// "does not start a replacement until a failed generation reports that it
// closed"
#[tokio::test]
async fn a_failed_generation_must_report_its_close_first() {
    let factory = Factory::new(
        failing_server("initialize failed"),
        Knobs {
            close_reports: false,
            ..Knobs::default()
        },
    );
    let (handle, recorder) = start(&factory, policy(2.0, 8.0, 2));
    wait_for(|| {
        factory
            .made()
            .first()
            .is_some_and(|made| made.closes.load(Ordering::SeqCst) > 0)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(factory.count(), 1);
    factory.made()[0].drop_connection();
    assert!(handle.ready().await.is_err());
    wait_for(|| factory.count() == 2).await;
    assert!(recorder.has(
        Level::Warn,
        "connection failed; retrying in 2ms (attempt 1/2)"
    ));
    for made in factory.made() {
        made.drop_connection();
    }
    handle.dispose().await;
}

// "stops reconnecting when a failed generation never reports that it
// closed"
#[tokio::test(start_paused = true)]
async fn a_failed_generation_that_never_closes_stops_reconnecting() {
    let factory = Factory::new(
        failing_server("initialize failed"),
        Knobs {
            close_reports: false,
            ..Knobs::default()
        },
    );
    let (handle, recorder) = start(&factory, policy(2.0, 8.0, 2));
    assert!(handle.ready().await.is_err());
    assert_eq!(factory.count(), 1);
    assert!(recorder.has(
        Level::Error,
        "mcp-client(srv): failed generation could not confirm transport closure — reconnect stopped to avoid overlapping server processes; reload the plugin or restart the Host to retry"
    ));
}

// negotiation-lifecycle.spec.ts: "stops retries when a failed probe cannot
// confirm transport cleanup"
#[tokio::test]
async fn a_probe_failure_whose_close_fails_stops_reconnecting() {
    let knobs = Knobs {
        start_error: Some(karaka_mcp_client::McpError::transport("probe failed")),
        ..Knobs::default()
    };
    knobs.failing_closes.store(2, Ordering::SeqCst);
    let factory = Factory::new(legacy_server(&[]), knobs);
    let (handle, recorder) = start(&factory, policy(1.0, 1.0, 3));
    assert!(handle.ready().await.is_err());
    assert_eq!(
        recorder.lines(Level::Error),
        [
            "mcp-client(srv): failed generation could not confirm transport closure — reconnect stopped to avoid overlapping server processes; reload the plugin or restart the Host to retry"
        ]
    );
    assert!(!recorder.has(Level::Warn, "connection failed; retrying"));
    assert_eq!(factory.count(), 1);
}

// negotiation-lifecycle.spec.ts: "retries failed HTTP probes without
// waiting for a Client close event", with a scripted 503.
#[tokio::test]
async fn failed_http_probes_are_retried_to_the_cap() {
    let factory = Factory::new(
        Arc::new(|_: &Value| Reply::Fail(common::http_error(503))),
        Knobs {
            kind: karaka_mcp_client::TransportKind::Http,
            ..Knobs::default()
        },
    );
    let (handle, recorder) = start(&factory, policy(1.0, 1.0, 3));
    let error = handle.ready().await.unwrap_err();
    assert_eq!(
        error.message,
        "Version negotiation failed: the server answered the probe with HTTP 503"
    );
    wait_for(|| recorder.has(Level::Error, "giving up after 3")).await;
    assert_eq!(factory.count(), 4);
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

// "dispose during the backoff wait cancels the pending reconnect"
#[tokio::test]
async fn disposing_during_the_backoff_cancels_it() {
    let factory = Factory::new(legacy_server(&["remote"]), Knobs::default());
    let (handle, recorder) = start(&factory, policy(60_000.0, 60_000.0, 5));
    handle.ready().await.unwrap();
    factory.made()[0].drop_connection();
    wait_for(|| recorder.has(Level::Warn, "reconnecting in 60000ms")).await;
    tokio::time::timeout(Duration::from_secs(2), handle.dispose())
        .await
        .expect("disposal returns promptly");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(factory.count(), 1);
    assert!(recorder.published().is_empty());
}

// "a transport close after dispose schedules nothing"
#[tokio::test]
async fn a_close_after_disposal_schedules_nothing() {
    let factory = Factory::new(legacy_server(&["remote"]), Knobs::default());
    let (handle, recorder) = start(&factory, policy(2.0, 8.0, 5));
    handle.ready().await.unwrap();
    handle.dispose().await;
    assert!(recorder.published().is_empty());
    assert_eq!(recorder.clears.load(Ordering::SeqCst), 1);
    factory.made()[0].drop_connection();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(factory.count(), 1);
    assert!(recorder.lines(Level::Warn).is_empty());
}

// "reconnect disabled keeps the registered tools and reports manual
// recovery", and its failed-initial-connect twin.
#[tokio::test]
async fn with_reconnect_off_a_lost_or_failed_connection_says_so() {
    let off = ReconnectPolicy {
        enabled: false,
        ..RECONNECT_DEFAULTS
    };
    let factory = Factory::new(legacy_server(&["remote"]), Knobs::default());
    let (handle, recorder) = start(&factory, off);
    handle.ready().await.unwrap();
    factory.made()[0].drop_connection();
    wait_for(|| recorder.has(Level::Error, "connection lost and reconnect is disabled")).await;
    assert!(recorder.has(
        Level::Error,
        "mcp-client(srv): connection lost and reconnect is disabled — registered tools will fail until an HMR reload or Host restart"
    ));
    assert_eq!(factory.count(), 1);
    assert_eq!(recorder.published(), ["remote"]);
    handle.dispose().await;

    let factory = Factory::new(failing_server("refused"), Knobs::default());
    let (handle, recorder) = start(&factory, off);
    assert!(handle.ready().await.is_err());
    assert!(recorder.has(
        Level::Error,
        "mcp-client(srv): connection failed and reconnect is disabled — no tools were registered; reload the plugin or restart the Host to connect"
    ));
    assert!(recorder.published().is_empty());
}

// "an uptime past the stability window resets the attempt budget"
#[tokio::test]
async fn an_uptime_past_the_window_resets_the_budget() {
    let factory = Factory::new(legacy_server(&["remote"]), Knobs::default());
    let (handle, recorder) = start(&factory, policy(2.0, 30.0, 1));
    handle.ready().await.unwrap();
    factory.made()[0].drop_connection();
    wait_for(|| factory.count() == 2 && handle.client().is_ok()).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    factory.made()[1].drop_connection();
    wait_for(|| factory.count() == 3 && handle.client().is_ok()).await;
    assert!(recorder.lines(Level::Error).is_empty());
    handle.dispose().await;
}

// "a crash loop with briefly successful connects still exhausts the cap"
#[tokio::test]
async fn a_crash_loop_still_exhausts_the_cap() {
    let factory = Factory::new(legacy_server(&["remote"]), Knobs::default());
    let (handle, recorder) = start(&factory, policy(2.0, 10_000.0, 1));
    handle.ready().await.unwrap();
    factory.made()[0].drop_connection();
    wait_for(|| factory.count() == 2 && handle.client().is_ok()).await;
    factory.made()[1].drop_connection();
    wait_for(|| {
        recorder.has(
            Level::Error,
            "giving up after 1 consecutive failed reconnect attempts",
        )
    })
    .await;
    assert_eq!(factory.count(), 2);
    wait_for(|| recorder.published().is_empty()).await;
    handle.dispose().await;
}

// "a connect rejection racing its own transport close schedules exactly
// one retry per attempt": every reconnect's server closes as its
// `initialize` fails.
#[tokio::test]
async fn a_failure_racing_its_own_close_retries_once_per_attempt() {
    let factory = Factory::new(legacy_server(&["remote"]), Knobs::default());
    let (handle, recorder) = start(&factory, policy(2.0, 8.0, 3));
    handle.ready().await.unwrap();
    let made = factory.clone();
    factory.set_script(Arc::new(move |message: &Value| {
        if message["method"] == "initialize"
            && let Some(last) = made.made().last()
        {
            last.drop_connection();
        }
        Reply::Error(-32601, "Method not found")
    }));
    factory.made()[0].drop_connection();
    wait_for(|| {
        recorder.has(
            Level::Error,
            "giving up after 3 consecutive failed reconnect attempts",
        )
    })
    .await;
    assert_eq!(factory.count(), 4);
    assert_eq!(
        recorder
            .lines(Level::Error)
            .iter()
            .filter(|line| line.contains("giving up"))
            .count(),
        1
    );
    handle.dispose().await;
}

// "a transport that closes during a resolving connect registers nothing
// from the dead generation"
#[tokio::test]
async fn a_close_during_the_handshake_registers_nothing() {
    let factory = Factory::new(legacy_server(&["remote"]), Knobs::default());
    let made = factory.clone();
    factory.set_script(Arc::new(move |message: &Value| {
        match message["method"].as_str() {
            Some("server/discover") => Reply::Error(-32601, "Method not found"),
            Some("initialize") => {
                let last = made.made().last().cloned();
                if let Some(last) = last {
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        last.drop_connection();
                    });
                }
                Reply::Silent
            }
            _ => Reply::Silent,
        }
    }));
    let (handle, recorder) = start(&factory, policy(10_000.0, 10_000.0, 1));
    assert!(handle.ready().await.is_err());
    assert_eq!(recorder.syncs.load(Ordering::SeqCst), 0);
    assert!(recorder.published().is_empty());
    handle.dispose().await;
}

// "dispose during an in-flight initial sync quiesces without leaking
// tools", and "keeps instructions withdrawn when disposal interrupts
// initial discovery".
#[tokio::test]
async fn disposing_during_the_first_sync_quiesces() {
    let factory = Factory::new(
        Arc::new(|message: &Value| match message["method"].as_str() {
            Some("server/discover") => Reply::Error(-32601, "Method not found"),
            Some("initialize") => Reply::Result(json!({
                "protocolVersion": "2025-11-25",
                "capabilities": { "tools": {} },
                "serverInfo": {},
                "instructions": "Instructions after discovery.",
            })),
            Some("tools/list") => Reply::Result(json!({ "tools": [{ "name": "remote" }] })),
            _ => Reply::Silent,
        }),
        Knobs::default(),
    );
    let recorder = Arc::new(Recorder::default());
    let gate = Arc::new(tokio::sync::Notify::new());
    *recorder.gate.lock().unwrap() = Some(Arc::clone(&gate));
    let handle = start_connection(
        ConnectionConfig::new("srv"),
        RECONNECT_DEFAULTS,
        factory.factory(),
        Arc::clone(&recorder) as _,
    );
    wait_for(|| recorder.syncs.load(Ordering::SeqCst) == 1).await;
    let disposing = tokio::spawn({
        let handle = handle.clone();
        async move { handle.dispose().await }
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    gate.notify_one();
    disposing.await.unwrap();
    assert_eq!(handle.instructions(), "");
    assert!(recorder.published().is_empty());
    assert_eq!(factory.count(), 1);
}

// The server's instructions, attributed, and their byte cap.
#[tokio::test]
async fn instructions_are_attributed_and_capped() {
    let instructed = |text: &'static str| -> common::Script {
        Arc::new(move |message: &Value| match message["method"].as_str() {
            Some("server/discover") => Reply::Error(-32601, "Method not found"),
            Some("initialize") => Reply::Result(json!({
                "protocolVersion": "2025-11-25", "capabilities": {}, "serverInfo": {},
                "instructions": text,
            })),
            _ => Reply::Silent,
        })
    };
    let factory = Factory::new(instructed("Use add.\n\n"), Knobs::default());
    let (handle, _) = start(&factory, RECONNECT_DEFAULTS);
    handle.ready().await.unwrap();
    assert_eq!(handle.instructions(), "### MCP server: srv\n\nUse add.");
    handle.dispose().await;
    assert_eq!(handle.instructions(), "");

    let factory = Factory::new(instructed("x"), Knobs::default());
    let recorder = Arc::new(Recorder::default());
    let mut config = ConnectionConfig::new("srv");
    config.max_instruction_bytes = 10;
    let handle = start_connection(
        config,
        ReconnectPolicy {
            enabled: false,
            ..RECONNECT_DEFAULTS
        },
        factory.factory(),
        Arc::clone(&recorder) as _,
    );
    let error = handle.ready().await.unwrap_err();
    assert_eq!(
        error.message,
        "mcp-client(srv): server instructions exceed maxInstructionBytes (10)"
    );
    assert_eq!(handle.instructions(), "");
}

// "a stale notification handler from a replaced generation is ignored",
// and a change heard from the current one re-syncs.
#[tokio::test]
async fn only_the_current_generations_changes_resync() {
    let factory = Factory::new(legacy_server(&["remote"]), Knobs::default());
    let (handle, recorder) = start(&factory, policy(2.0, 8.0, 5));
    handle.ready().await.unwrap();
    let first = factory.made()[0].clone();
    factory.set_script(legacy_server(&["next"]));
    first.drop_connection();
    wait_for(|| recorder.published() == ["next"]).await;
    let syncs = recorder.syncs.load(Ordering::SeqCst);
    first.push(json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(recorder.syncs.load(Ordering::SeqCst), syncs);

    factory.made()[1].set_script(legacy_server(&["changed"]));
    factory.made()[1]
        .push(json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }));
    wait_for(|| recorder.published() == ["changed"]).await;
    assert!(recorder.has(
        Level::Info,
        "mcp-client(srv): tool list changed, re-syncing"
    ));
    handle.dispose().await;
}

// The connected generation only: a replacement still negotiating is
// disconnected. (dsh: "rejects resource reads while a replacement
// connection is still negotiating".)
#[tokio::test]
async fn a_replacement_still_negotiating_is_disconnected() {
    let factory = Factory::new(legacy_server(&[]), Knobs::default());
    let (handle, _) = start(&factory, policy(2.0, 8.0, 2));
    handle.ready().await.unwrap();
    assert!(handle.client().is_ok());
    factory.set_script(Arc::new(|_: &Value| Reply::Silent));
    factory.made()[0].drop_connection();
    wait_for(|| factory.count() == 2).await;
    assert_eq!(
        handle.client().err().unwrap().message,
        "mcp-client(srv): server is disconnected"
    );
    handle.dispose().await;
}

// "resolveReconnectPolicy"
#[test]
fn the_reconnect_policy_is_resolved_in_dshs_words() {
    let path = "mcp-client(srv): reconnect";
    assert_eq!(
        resolve_reconnect_policy(None, path).unwrap(),
        RECONNECT_DEFAULTS
    );
    assert_eq!(
        resolve_reconnect_policy(
            Some(
                &json!({ "enabled": false, "initialDelayMs": 1, "maxDelayMs": 2, "maxAttempts": 7 })
            ),
            path
        )
        .unwrap(),
        ReconnectPolicy {
            enabled: false,
            initial_delay_ms: 1.0,
            max_delay_ms: 2.0,
            max_attempts: 7,
        }
    );
    let refused = |config: Value| resolve_reconnect_policy(Some(&config), path).unwrap_err();
    assert_eq!(
        refused(json!({ "jitterRatio": 0.5 })),
        "mcp-client(srv): reconnect.jitterRatio is not a reconnect option"
    );
    assert_eq!(
        refused(json!({ "initialDelayMs": 0 })),
        "mcp-client(srv): reconnect.initialDelayMs must be a positive finite number no greater than 2147483647"
    );
    assert!(refused(json!({ "initialDelayMs": "soon" })).contains("initialDelayMs"));
    assert_eq!(
        refused(json!({ "maxDelayMs": -1 })),
        "mcp-client(srv): reconnect.maxDelayMs must be a positive finite number no greater than 2147483647"
    );
    assert_eq!(
        refused(json!({ "initialDelayMs": 100, "maxDelayMs": 5 })),
        "mcp-client(srv): reconnect.initialDelayMs must be less than or equal to maxDelayMs"
    );
    for attempts in [json!(0), json!(1.5)] {
        assert_eq!(
            refused(json!({ "maxAttempts": attempts })),
            "mcp-client(srv): reconnect.maxAttempts must be a positive integer"
        );
    }
}
