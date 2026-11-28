//! Browser access over loopback HTTP and a real WebSocket. (Karaka: the
//! browser cases of `routes.spec.ts`, `socket.spec.ts`, and the browser
//! cases of `interactions.spec.ts`.)
//!
//! Not ported:
//! - `browser.spec.ts`, which tests the browser's own client
//!   (`browser.ts`); that code runs in the browser and stays Karaka's;
//! - disposal (the kernel unloads nothing);
//! - the stubbed failures a real controller can't produce, and a request
//!   URL Node omits;
//! - an operation cut when its credential expires mid-request: a JSON
//!   operation here runs to its end, and an SSE stream ends at expiry.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Ended, ORIGIN, Opened, Options, Server, browser, start};
use karaka_agent::Agent;
use karaka_session::{SessionEvent, SessionId, TurnStart};
use karaka_transport_http::{BrowserConfig, TransportConfig};
use karaka_user_approval::{ApprovalOutcome, ApprovalRequest, ApprovalService};
use karaka_user_questions::{
    AskUserQuestionItem, AskUserQuestionOption, QuestionRequest, UserQuestions,
};
use serde_json::{Value, json};

async fn server_with(config: BrowserConfig) -> Server {
    start(Options {
        browser: Some(config),
        config: TransportConfig {
            path: "/api".into(),
            handle_questions: false,
            ..TransportConfig::default()
        },
        ..Options::default()
    })
    .await
}

async fn with_chat() -> (Server, String) {
    let server = server_with(browser()).await;
    let token = server.signer.token("user", "chat", 120);
    let created = server
        .browser(
            "applicationCreate",
            &json!({ "chatId": "chat", "agentId": "main" }),
            &token,
        )
        .await;
    assert_eq!(created.status, 200, "{}", created.body);
    (server, token)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn origins_preflight_credentials_and_post_only() {
    let server = server_with(browser()).await;
    let token = server.signer.token("user", "chat", 120);
    let foreign = server
        .with_headers(
            "POST",
            "/browser/applicationAgents",
            "{}",
            &[("Origin", "https://evil.example")],
        )
        .await;
    assert_eq!(
        (foreign.status, foreign.body.as_str()),
        (
            403,
            r#"{"code":"ORIGIN_FORBIDDEN","message":"Browser origin is forbidden"}"#
        )
    );
    assert!(!foreign.head.contains("access-control-allow-origin"));
    let preflight = server
        .with_headers(
            "OPTIONS",
            "/browser/applicationAgents",
            "",
            &[("Origin", ORIGIN)],
        )
        .await;
    assert_eq!(preflight.status, 204);
    assert!(
        preflight
            .head
            .contains("access-control-allow-origin: https://app.example")
    );
    assert!(preflight.head.contains("vary: origin"));
    assert!(
        preflight
            .head
            .contains("access-control-allow-methods: post")
    );
    assert!(
        preflight
            .head
            .contains("access-control-allow-headers: authorization,content-type")
    );
    let authorization = format!("Bearer {token}");
    let get = server
        .with_headers(
            "GET",
            "/browser/applicationAgents",
            "",
            &[("Origin", ORIGIN), ("Authorization", &authorization)],
        )
        .await;
    assert_eq!(
        (get.status, get.body.as_str()),
        (
            405,
            r#"{"code":"METHOD_FORBIDDEN","message":"POST required"}"#
        )
    );
    let anonymous = server
        .with_headers(
            "POST",
            "/browser/applicationAgents",
            "{}",
            &[("Origin", ORIGIN)],
        )
        .await;
    assert_eq!(
        (anonymous.status, anonymous.body.as_str()),
        (
            401,
            r#"{"code":"UNAUTHORIZED","message":"Invalid browser credential"}"#
        )
    );
    assert!(
        anonymous
            .head
            .contains("access-control-allow-origin: https://app.example")
    );
    assert_eq!(
        server
            .browser("applicationAgents", &json!({}), "forged")
            .await
            .status,
        401
    );
    let missing = server.browser("missing", &json!({}), &token).await;
    assert_eq!(
        (missing.status, missing.body.as_str()),
        (
            403,
            r#"{"code":"METHOD_FORBIDDEN","message":"Browser method is unavailable"}"#
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_methods_take_their_owner_from_the_signed_claims() {
    let server = server_with(browser()).await;
    let token = server.signer.token("user", "chat", 120);
    let agents = server
        .browser("applicationAgents", &json!({}), &token)
        .await;
    assert_eq!(
        agents.body,
        r#"{"ok":true,"value":[{"id":"main","name":"Main"}]}"#
    );
    let created = server
        .browser(
            "applicationCreate",
            &json!({ "chatId": "chat", "agentId": "main" }),
            &token,
        )
        .await;
    assert_eq!(
        created.body,
        r#"{"ok":true,"value":{"chatId":"chat","agentId":"main"}}"#
    );
    let owner_field = server
        .browser(
            "applicationCreate",
            &json!({ "chatId": "chat", "agentId": "main", "userId": "foreign" }),
            &token,
        )
        .await;
    assert_eq!(
        (owner_field.status, owner_field.body.as_str()),
        (
            400,
            r#"{"ok":false,"error":{"code":"gateway/bad-request","message":"Invalid request","details":{}}}"#
        )
    );
    // Another chat than the signed one, and another preset, are refused.
    let other = server
        .browser(
            "applicationCreate",
            &json!({ "chatId": "other", "agentId": "main" }),
            &token,
        )
        .await;
    assert_eq!(
        (other.status, other.body.as_str()),
        (
            403,
            r#"{"ok":false,"error":{"code":"gateway/forbidden","message":"Chat access is forbidden","details":{}}}"#
        )
    );
    let plain = server
        .browser(
            "applicationCreate",
            &json!({ "chatId": "chat", "agentId": "plain" }),
            &token,
        )
        .await;
    assert_eq!(plain.status, 403);

    server.model.replies.lock().unwrap().push_back("Hi.".into());
    let prompt = server
        .browser(
            "applicationPrompt",
            &json!({ "chatId": "chat", "requestId": "r", "content": [{ "type": "text", "text": "hi" }] }),
            &token,
        )
        .await;
    assert_eq!(
        prompt.body,
        r#"{"ok":true,"value":{"accepted":true,"duplicate":false}}"#
    );
    server.idle("chat").await;
    // History is the raw stored events.
    let history = server
        .browser("applicationHistory", &json!({ "chatId": "chat" }), &token)
        .await
        .json();
    assert_eq!(history["ok"], true);
    let types: Vec<&str> = history["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert!(
        types.contains(&"user/message") && types.contains(&"turn/end"),
        "{types:?}"
    );
    let cancel = server
        .browser("applicationCancel", &json!({ "chatId": "chat" }), &token)
        .await;
    assert_eq!(cancel.body, r#"{"ok":true,"value":{"accepted":true}}"#);
    let missing = server
        .browser(
            "result",
            &json!({ "clientId": "missing", "eventId": "missing", "outcome": { "kind": "next" } }),
            &token,
        )
        .await;
    assert_eq!(missing.status, 403);
    // Another user, signed for the same chat, is refused.
    let foreign = server.signer.token("foreign", "chat", 120);
    assert_eq!(
        server
            .browser("applicationHistory", &json!({ "chatId": "chat" }), &foreign)
            .await
            .status,
        403
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follow_over_http_opens_with_the_full_snapshot() {
    let (server, token) = with_chat().await;
    let authorization = format!("Bearer {token}");
    let mut stream = server
        .stream_with(
            "/browser/applicationFollow",
            &json!({ "chatId": "chat" }),
            &[("Origin", ORIGIN), ("Authorization", &authorization)],
        )
        .await;
    assert!(
        stream
            .head
            .contains("content-type: text/event-stream; charset=utf-8"),
        "{}",
        stream.head
    );
    let first = stream.next().await.unwrap();
    assert_eq!(first["ok"], true);
    let snapshot = &first["value"];
    assert_eq!(
        snapshot.as_object().unwrap().keys().collect::<Vec<_>>(),
        [
            "type",
            "header",
            "hasMore",
            "projections",
            "cursor",
            "records"
        ]
    );
    assert_eq!(snapshot["header"]["id"], "chat");
    assert_eq!(snapshot["header"]["agentPreset"], "main");
    assert_eq!(snapshot["header"]["version"], 4);
    assert_eq!(snapshot["hasMore"], false);
    assert_eq!(
        snapshot["projections"],
        json!({ "asOfSeq": snapshot["cursor"], "values": {} })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_method_is_unavailable() {
    let server = server_with(BrowserConfig {
        methods: vec!["applicationHistory".into()],
        ..browser()
    })
    .await;
    let token = server.signer.token("user", "chat", 120);
    let refused = server
        .browser("applicationAgents", &json!({}), &token)
        .await;
    assert_eq!(refused.status, 403);
    let mut socket = server.open().await;
    socket.hello(&token).await;
    socket
        .send(&json!({ "type": "follow", "id": "a", "chatId": "chat" }))
        .await;
    let frame = socket.read().await.unwrap();
    assert_eq!(frame["result"]["error"]["code"], "gateway/forbidden");
    assert_eq!(
        socket.read().await.unwrap(),
        json!({ "type": "end", "id": "a" })
    );
}

#[test]
fn the_browser_config_is_checked_in_karakas_words() {
    let base = "/api";
    let check = |config: BrowserConfig| config.check(base).unwrap_err();
    assert_eq!(
        check(BrowserConfig {
            origins: vec![],
            ..browser()
        }),
        "Browser transport requires explicit origins"
    );
    for agents in [vec![], vec![String::new()], vec![" ".to_owned()]] {
        assert_eq!(
            check(BrowserConfig {
                agent_ids: agents,
                ..browser()
            }),
            "Browser transport requires explicit Agent presets"
        );
    }
    for path in ["/api", "/api/browser"] {
        assert_eq!(
            check(BrowserConfig {
                path: path.into(),
                ..browser()
            }),
            "Browser and backend paths must be disjoint"
        );
    }
    assert!(
        check(BrowserConfig {
            path: "/".into(),
            ..browser()
        })
        .contains("path must start")
    );
    assert_eq!(
        BrowserConfig {
            path: "/browser".into(),
            ..browser()
        }
        .check("/browser/api")
        .unwrap_err(),
        "Browser and backend paths must be disjoint"
    );
    assert_eq!(
        check(BrowserConfig {
            heartbeat_ms: 0,
            ..browser()
        }),
        "browserHeartbeatMs must be a positive safe integer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_socket_is_refused_while_starting_and_from_other_origins() {
    let server = start(Options {
        browser: Some(browser()),
        ready: false,
        ..Options::default()
    })
    .await;
    assert!(matches!(
        server.socket(Some(ORIGIN)).await,
        Opened::Refused(503)
    ));
    assert_eq!(
        server
            .browser("applicationAgents", &json!({}), "x")
            .await
            .status,
        503
    );
    server
        .runtime
        .get::<karaka_startup::KarakaStartup>()
        .commit();
    assert!(matches!(
        server.socket(Some("https://evil.example")).await,
        Opened::Refused(403)
    ));
    assert!(matches!(server.socket(None).await, Opened::Refused(403)));
    assert!(matches!(
        server.socket(Some(ORIGIN)).await,
        Opened::Socket(_)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_socket_must_say_hello_first_with_a_good_credential() {
    let server = server_with(browser()).await;
    let token = server.signer.token("user", "chat", 120);
    let mut binary = server.open().await;
    binary
        .frame(karaka_transport_http::ws::OP_BINARY, b"{}")
        .await;
    assert_eq!(
        binary.ended().await,
        Ended::Closed(Some(4400), "Invalid message".into())
    );
    let mut invalid = server.open().await;
    invalid
        .frame(karaka_transport_http::ws::OP_TEXT, b"not json")
        .await;
    assert_eq!(
        invalid.ended().await,
        Ended::Closed(Some(4400), "Invalid message".into())
    );
    let mut unknown = server.open().await;
    unknown.send(&json!({ "type": "subscribe" })).await;
    assert_eq!(
        unknown.ended().await,
        Ended::Closed(Some(4400), "Invalid message".into())
    );
    let mut early = server.open().await;
    early
        .send(&json!({ "type": "follow", "id": "1", "chatId": "chat" }))
        .await;
    assert_eq!(
        early.ended().await,
        Ended::Closed(Some(4400), "Authenticate first".into())
    );
    let mut refused = server.open().await;
    refused
        .send(&json!({ "type": "hello", "credential": "unsigned" }))
        .await;
    assert_eq!(
        refused.ended().await,
        Ended::Closed(Some(4401), "Invalid browser credential".into())
    );
    let mut repeated = server.open().await;
    let ready = repeated.hello(&token).await;
    assert_eq!(ready["heartbeatMs"], 15_000);
    assert!(ready["clientId"].is_string());
    assert!(ready["host"]["home"].is_string());
    repeated
        .send(&json!({ "type": "hello", "credential": token }))
        .await;
    assert_eq!(
        repeated.ended().await,
        Ended::Closed(Some(4400), "Already authenticated".into())
    );
    // A chat the credential's preset can't reach is forbidden.
    server
        .runtime
        .get::<karaka_session::SessionStore>()
        .create_with(karaka_session::SessionHeader {
            agent_preset: Some("plain".into()),
            ..karaka_session::SessionHeader::new(SessionId::new("plain-chat"), 1)
        })
        .unwrap();
    let mut denied = server.open().await;
    denied.send(&json!({ "type": "hello", "credential": server.signer.token("user", "plain-chat", 120) })).await;
    assert_eq!(
        denied.ended().await,
        Ended::Closed(Some(4403), "Chat access is forbidden".into())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_message_closes_with_1009() {
    let server = start(Options {
        browser: Some(browser()),
        config: TransportConfig {
            path: "/api".into(),
            max_body_bytes: 64,
            ..TransportConfig::default()
        },
        ..Options::default()
    })
    .await;
    let mut socket = server.open().await;
    socket
        .send(&json!({ "type": "hello", "credential": "x".repeat(100) }))
        .await;
    assert_eq!(
        socket.ended().await,
        Ended::Closed(Some(1009), String::new())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follows_stream_snapshot_first_end_and_stop() {
    let (server, token) = with_chat().await;
    let mut socket = server.open().await;
    socket.hello(&token).await;
    socket
        .send(&json!({ "type": "follow", "id": "a", "chatId": "chat" }))
        .await;
    let snapshot = socket.read().await.unwrap();
    assert_eq!(snapshot["type"], "frame");
    assert_eq!(snapshot["id"], "a");
    assert_eq!(snapshot["result"]["value"]["type"], "snapshot");
    // A live event arrives as an `event` frame.
    server
        .model
        .replies
        .lock()
        .unwrap()
        .push_back("Live.".into());
    server
        .browser(
            "applicationPrompt",
            &json!({ "chatId": "chat", "requestId": "r", "content": [{ "type": "text", "text": "go" }] }),
            &token,
        )
        .await;
    let mut kinds = Vec::new();
    loop {
        let frame = socket.read().await.unwrap();
        let value = &frame["result"]["value"];
        let kind = match value["type"].as_str().unwrap() {
            "event" => value["event"]["type"].as_str().unwrap().to_owned(),
            other => other.to_owned(),
        };
        kinds.push(kind.clone());
        if kind == "turn/end" {
            break;
        }
    }
    assert!(kinds.contains(&"text-delta".to_owned()), "{kinds:?}");
    assert!(kinds.contains(&"assistant/message".to_owned()), "{kinds:?}");
    // A duplicate id closes; first, a stop ends a follow with `end`.
    socket.send(&json!({ "type": "stop", "id": "a" })).await;
    socket
        .send(&json!({ "type": "stop", "id": "unknown" }))
        .await;
    assert_eq!(
        socket.read().await.unwrap(),
        json!({ "type": "end", "id": "a" })
    );
    socket
        .send(&json!({ "type": "follow", "id": "b", "chatId": "chat" }))
        .await;
    assert_eq!(socket.read().await.unwrap()["id"], "b");
    socket
        .send(&json!({ "type": "follow", "id": "b", "chatId": "chat" }))
        .await;
    assert_eq!(
        socket.ended().await,
        Ended::Closed(Some(4400), "Duplicate follow id".into())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pings_pongs_and_silence() {
    let server = server_with(BrowserConfig {
        heartbeat_ms: 60,
        ..browser()
    })
    .await;
    let token = server.signer.token("user", "chat", 120);
    let mut live = server.open().await;
    assert_eq!(live.hello(&token).await["heartbeatMs"], 60);
    assert_eq!(live.read().await.unwrap(), json!({ "type": "ping" }));
    live.send(&json!({ "type": "pong" })).await;
    assert_eq!(live.read().await.unwrap(), json!({ "type": "ping" }));
    live.send(&json!({ "type": "pong" })).await;
    assert_eq!(live.read().await.unwrap(), json!({ "type": "ping" }));
    // This client never answers a ping.
    let mut silent = server.open().await;
    silent.hello(&token).await;
    assert_eq!(silent.read().await.unwrap(), json!({ "type": "ping" }));
    assert_eq!(silent.ended().await, Ended::Cut);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_socket_that_never_says_hello_or_whose_credential_expires_is_closed() {
    let server = server_with(BrowserConfig {
        heartbeat_ms: 40,
        ..browser()
    })
    .await;
    let mut idle = server.open().await;
    assert_eq!(
        idle.ended().await,
        Ended::Closed(Some(4401), "Authentication timed out".into())
    );
    let server = server_with(browser()).await;
    let mut expiring = server.open().await;
    expiring
        .hello(&server.signer.token("user", "chat", 1))
        .await;
    assert_eq!(
        expiring.ended().await,
        Ended::Closed(Some(4401), "Browser credential expired".into())
    );
}

fn question() -> Vec<AskUserQuestionItem> {
    vec![AskUserQuestionItem {
        options: Some(vec![AskUserQuestionOption::new("Yes")]),
        ..AskUserQuestionItem::new("choice", "Continue?")
    }]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn questions_and_approvals_reach_the_owners_browser_and_replay() {
    let (server, token) = with_chat().await;
    let agent: Arc<dyn Agent> = server.agent("chat");
    let mut first = server.open().await;
    first.hello(&token).await;
    let questions = server.runtime.get::<UserQuestions>();
    let asking = tokio::spawn({
        let (questions, agent) = (Arc::clone(&questions), Arc::clone(&agent));
        async move {
            questions
                .ask(QuestionRequest {
                    agent: Some(agent),
                    ..QuestionRequest::new(question())
                })
                .await
        }
    });
    let event = first.read().await.unwrap();
    assert_eq!(
        event.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["type", "eventId", "chatId", "event", "request"]
    );
    assert_eq!(event["chatId"], "chat");
    assert_eq!(event["event"], "user-questions/request");
    assert_eq!(event["request"]["agent"], json!({ "id": "chat" }));
    assert_eq!(event["request"]["questions"][0]["question"], "Continue?");
    // It replays to a reconnected socket.
    drop(first);
    let mut second = server.open().await;
    let ready = second.hello(&token).await;
    let replayed = second.read().await.unwrap();
    assert_eq!(replayed["eventId"], event["eventId"]);
    let reply = |client: &Value, value: Value| json!({ "clientId": client, "eventId": event["eventId"], "outcome": { "kind": "result", "value": value } });
    let bad = server
        .browser(
            "result",
            &reply(
                &ready["clientId"],
                json!({ "answers": [{ "id": "choice", "selected": "Yes" }] }),
            ),
            &token,
        )
        .await;
    assert_eq!(bad.status, 400);
    let foreign = server.signer.token("foreign", "chat", 120);
    let answers =
        json!({ "answers": [{ "id": "choice", "selected": ["Yes"], "custom": "Proceed" }] });
    assert_eq!(
        server
            .browser(
                "result",
                &reply(&ready["clientId"], answers.clone()),
                &foreign
            )
            .await
            .status,
        403
    );
    let accepted = server
        .browser(
            "result",
            &reply(&ready["clientId"], answers.clone()),
            &token,
        )
        .await;
    assert_eq!(
        (accepted.status, accepted.body.as_str()),
        (200, r#"{"ok":true,"value":null}"#)
    );
    let answer = asking.await.unwrap().unwrap();
    assert_eq!(answer.answers[0].custom.as_deref(), Some("Proceed"));
    assert_eq!(
        second.read().await.unwrap(),
        json!({ "type": "cancel", "eventId": event["eventId"] })
    );
    assert_eq!(
        server
            .browser("result", &reply(&ready["clientId"], answers), &token)
            .await
            .status,
        403
    );

    // An approval, in an open turn.
    server
        .runtime
        .get::<karaka_session::SessionStore>()
        .get(&SessionId::new("chat"))
        .unwrap()
        .append(SessionEvent::TurnStart(TurnStart {
            turn: 99,
            extra: Default::default(),
        }))
        .unwrap();
    let approvals = server.runtime.get::<ApprovalService>();
    let approving = tokio::spawn({
        let (approvals, agent) = (Arc::clone(&approvals), Arc::clone(&agent));
        async move {
            let request = ApprovalRequest {
                reason: Some("Run command".into()),
                ..ApprovalRequest::new(agent, "bash")
            };
            approvals.request(&request).await
        }
    });
    let asked = second.read().await.unwrap();
    assert_eq!(asked["event"], "approval/request");
    assert_eq!(
        asked["request"],
        json!({ "agent": { "id": "chat" }, "toolName": "bash", "reason": "Run command" })
    );
    let invalid = json!({ "clientId": ready["clientId"], "eventId": asked["eventId"], "outcome": { "kind": "result", "value": "invalid" } });
    assert_eq!(server.browser("result", &invalid, &token).await.status, 400);
    let allowed = json!({ "clientId": ready["clientId"], "eventId": asked["eventId"], "outcome": { "kind": "result", "value": "allowed-once" } });
    assert_eq!(server.browser("result", &allowed, &token).await.status, 200);
    assert_eq!(
        approving.await.unwrap().unwrap(),
        ApprovalOutcome::AllowedOnce
    );
    assert_eq!(second.read().await.unwrap()["type"], "cancel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_question_waits_until_every_recipient_delegates() {
    let (server, token) = with_chat().await;
    let authorization = format!("Bearer {token}");
    let headers = [
        ("Origin", ORIGIN),
        ("Authorization", authorization.as_str()),
    ];
    let mut first = server
        .stream_with("/browser/events", &json!({}), &headers)
        .await;
    let mut second = server
        .stream_with("/browser/events", &json!({}), &headers)
        .await;
    let one = first.next().await.unwrap();
    let two = second.next().await.unwrap();
    assert_eq!(one["type"], "ready");
    assert!(one.get("heartbeatMs").is_none());
    let questions = server.runtime.get::<UserQuestions>();
    let agent = server.agent("chat");
    let asking = tokio::spawn(async move {
        questions
            .ask(QuestionRequest {
                agent: Some(agent),
                ..QuestionRequest::new(question())
            })
            .await
    });
    let event = first.next().await.unwrap();
    assert_eq!(second.next().await.unwrap(), event);
    for ready in [&one, &two] {
        let reply = json!({ "clientId": ready["clientId"], "eventId": event["eventId"], "outcome": { "kind": "next" } });
        assert_eq!(server.browser("result", &reply, &token).await.status, 200);
    }
    // Every browser delegated, and no answerer is left.
    let failure = asking.await.unwrap().unwrap_err();
    assert_eq!(failure.code(), Some("NO_PROVIDER"));
    assert_eq!(
        first.next().await.unwrap(),
        json!({ "type": "cancel", "eventId": event["eventId"] })
    );
    let _ = Duration::ZERO;
}
