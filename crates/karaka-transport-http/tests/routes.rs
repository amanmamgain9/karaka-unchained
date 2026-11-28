//! The application routes over a real runtime and loopback HTTP. (Karaka:
//! `transport-http/tests/routes.spec.ts`, `application.spec.ts`,
//! `http.spec.ts` and the backend cases of `interactions.spec.ts`.)
//!
//! Karaka's specs stub the controller under the routes, and the services
//! under the controller; here both run for real, so each case checks the
//! wire as Play This Life's app reads it. Not ported:
//! - the browser cases (`browser*.ts`, step 4) and the approval cases,
//!   which reach only browsers;
//! - the cases of Karaka's attachment store (none here, so an image is
//!   always refused);
//! - disposal: the kernel unloads nothing, and `serve` closes the server;
//! - the fixtures' mocked failures (a follow ending before its snapshot, a
//!   listing that hangs until its client leaves), which a real controller
//!   can't produce;
//! - the `ToolArgs`, Node `drain` and listener-release cases of `http.spec`,
//!   which are Node's stream mechanics.

mod common;

use common::{CREDENTIAL, Options, identity, start, with};
use karaka_agent::Agent;
use karaka_identity::{ApplicationId, ApplicationOwner, TenantId, UserId};
use karaka_session::SessionId;
use karaka_transport_http::TransportConfig;
use karaka_user_questions::{
    AskUserQuestionItem, AskUserQuestionOption, QuestionRequest, UserQuestions,
};
use serde_json::{Value, json};

const JSON: &str = "content-type: application/json; charset=utf-8";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routes_wait_for_startup_and_need_the_server_credential() {
    let server = start(Options {
        ready: false,
        ..Options::default()
    })
    .await;
    let starting = server.get("/api/agents").await;
    assert_eq!(starting.status, 503);
    assert_eq!(
        starting.body,
        r#"{"code":"STARTING","message":"Application is starting"}"#
    );
    assert!(starting.head.contains(JSON), "{}", starting.head);
    server
        .runtime
        .get::<karaka_startup::KarakaStartup>()
        .commit();
    let agents = server.get("/api/agents").await;
    assert_eq!(agents.status, 200);
    assert_eq!(
        agents.body,
        r#"[{"id":"main","name":"Main"},{"id":"plain","name":"plain"}]"#
    );
    let refused = server
        .raw("GET", "/api/agents", "", Some("Bearer wrong"))
        .await;
    assert_eq!(refused.status, 401);
    assert_eq!(
        refused.body,
        r#"{"code":"UNAUTHORIZED","message":"Invalid server credential"}"#
    );
    assert_eq!(server.raw("GET", "/api/agents", "", None).await.status, 401);
    let missing = server.get("/api/missing").await;
    assert_eq!(missing.status, 404);
    assert_eq!(
        missing.body,
        r#"{"code":"NOT_FOUND","message":"Route not found"}"#
    );
    assert_eq!(server.get("/api/chats/chat/history").await.status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_bodies_and_path_escapes_are_refused() {
    let server = start(Options {
        config: TransportConfig {
            path: "/api".into(),
            max_body_bytes: 128,
            handle_questions: false,
        },
        ..Options::default()
    })
    .await;
    let invalid = r#"{"code":"BAD_REQUEST","message":"Invalid request"}"#;
    for body in ["", "{", "null", "[]", "\"text\"", "1", "{}"] {
        let answer = server
            .raw("POST", "/api/chats", body, Some(CREDENTIAL))
            .await;
        assert_eq!(
            (answer.status, answer.body.as_str()),
            (400, invalid),
            "{body}"
        );
    }
    // An unknown key, as a strict schema refuses it.
    let extra = with(json!({ "chatId": "c", "agentId": "main", "colour": "red" }));
    assert_eq!(server.post("/api/chats", &extra).await.status, 400);
    let large = json!({ "large": "x".repeat(200) });
    assert_eq!(server.post("/api/chats", &large).await.status, 400);
    let escape = server.post("/api/chats/%FF/history", &identity()).await;
    assert_eq!(escape.status, 500);
    assert_eq!(
        escape.body,
        r#"{"code":"INTERNAL_ERROR","message":"Internal server error"}"#
    );
    // A multibyte body exactly at the limit is read whole.
    let mut exact = with(json!({ "chatId": "c", "agentId": "main" }));
    let short = exact.to_string().len();
    exact["chatId"] = json!(format!("{}{}", "é".repeat((128 - short) / 2), "c"));
    let text = exact.to_string();
    assert!(text.len() <= 128, "{}", text.len());
    assert_ne!(
        server
            .raw("POST", "/api/chats", &text, Some(CREDENTIAL))
            .await
            .status,
        400
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chat_is_created_prompted_followed_and_read_back() {
    let server = start(Options::default()).await;
    server
        .model
        .replies
        .lock()
        .unwrap()
        .push_back("Hello there.".into());
    let created = server
        .post(
            "/api/chats",
            &with(json!({ "agentId": "main", "chatId": "chat" })),
        )
        .await;
    assert_eq!(
        (created.status, created.body.as_str()),
        (201, r#"{"chatId":"chat","agentId":"main"}"#)
    );
    assert!(created.head.contains(JSON));
    // Again, the same; another preset conflicts; another user is refused.
    assert_eq!(
        server
            .post(
                "/api/chats",
                &with(json!({ "agentId": "main", "chatId": "chat" }))
            )
            .await
            .status,
        201
    );
    let conflict = server
        .post(
            "/api/chats",
            &with(json!({ "agentId": "plain", "chatId": "chat" })),
        )
        .await;
    assert_eq!(
        (conflict.status, conflict.body.as_str()),
        (
            409,
            r#"{"code":"CHAT_CONFLICT","message":"Chat state conflicts with this operation"}"#
        )
    );
    let foreign =
        json!({ "tenantId": "tenant", "userId": "foreign", "agentId": "main", "chatId": "chat" });
    let forbidden = server.post("/api/chats", &foreign).await;
    assert_eq!(
        (forbidden.status, forbidden.body.as_str()),
        (
            403,
            r#"{"code":"CHAT_FORBIDDEN","message":"Chat access is forbidden"}"#
        )
    );
    let unknown = server
        .post(
            "/api/chats",
            &with(json!({ "agentId": "ghost", "chatId": "other" })),
        )
        .await;
    assert_eq!(
        (unknown.status, unknown.body.as_str()),
        (
            404,
            r#"{"code":"AGENT_NOT_FOUND","message":"Agent not found"}"#
        )
    );

    // The follow opens with the snapshot.
    let mut events = server.stream("/api/chats/chat/stream", &identity()).await;
    assert!(events.head.starts_with("http/1.1 200"), "{}", events.head);
    assert!(
        events
            .head
            .contains("content-type: text/event-stream; charset=utf-8")
    );
    assert!(
        events
            .head
            .contains("cache-control: no-cache, no-transform")
    );
    let snapshot = events.next().await.unwrap();
    assert_eq!(snapshot["type"], "snapshot");

    let content = json!([{ "type": "text", "text": "hi" }]);
    let receipt = server
        .post(
            "/api/chats/chat/messages",
            &with(json!({ "requestId": "r1", "content": content })),
        )
        .await;
    assert_eq!(
        (receipt.status, receipt.body.as_str()),
        (
            202,
            r#"{"chatId":"chat","requestId":"r1","accepted":true,"duplicate":false}"#
        )
    );
    let live = events.until("turn-end").await;
    let kinds: Vec<&str> = live
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"user-message"), "{kinds:?}");
    assert!(kinds.contains(&"text-delta"), "{kinds:?}");
    assert!(kinds.contains(&"assistant-message"), "{kinds:?}");
    let user = live
        .iter()
        .find(|event| event["type"] == "user-message")
        .unwrap();
    assert_eq!(
        user["content"]["source"],
        json!({ "kind": "user", "rpcId": "r1" })
    );
    assert_eq!(
        user["content"]["content"],
        json!([{ "type": "text", "text": "hi" }])
    );
    let delta = live
        .iter()
        .find(|event| event["type"] == "text-delta")
        .unwrap();
    assert_eq!(
        delta.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["type", "cursor", "text"]
    );
    let end = live.last().unwrap();
    assert_eq!(end["reason"], json!({ "kind": "completed" }));
    // Every durable event's cursor follows the last.
    let cursors: Vec<i64> = live
        .iter()
        .filter(|event| event["type"] != "text-delta")
        .map(|event| event["cursor"].as_i64().unwrap())
        .collect();
    assert!(
        cursors.windows(2).all(|pair| pair[0] < pair[1]),
        "{cursors:?}"
    );
    server.idle("chat").await;

    // A duplicate request id is acknowledged, not admitted again.
    let duplicate = server
        .post(
            "/api/chats/chat/messages",
            &with(json!({ "requestId": "r1", "content": content })),
        )
        .await;
    assert_eq!(
        duplicate.body,
        r#"{"chatId":"chat","requestId":"r1","accepted":true,"duplicate":true}"#
    );

    // The history projects the transcript; its envelope is the SDK's.
    let history = server.post("/api/chats/chat/history", &identity()).await;
    assert_eq!(history.status, 200);
    let history = history.json();
    assert_eq!(history["chatId"], "chat");
    let events_kinds: Vec<&str> = history["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        events_kinds,
        ["user-message", "assistant-message", "turn-end"]
    );
    let assistant = &history["events"][1];
    assert_eq!(
        assistant["content"]["content"],
        json!([{ "type": "text", "text": "Hello there." }])
    );
    let last = history["events"][2]["cursor"].as_i64().unwrap();

    // A cursor past the chat is refused; at the end, only the snapshot.
    let past = server
        .post(
            "/api/chats/chat/stream",
            &with(json!({ "cursor": last + 5 })),
        )
        .await;
    assert_eq!(
        (past.status, past.body.as_str()),
        (400, r#"{"code":"BAD_REQUEST","message":"Invalid request"}"#)
    );
    let mut caught_up = server
        .stream("/api/chats/chat/stream", &with(json!({ "cursor": last })))
        .await;
    let first = caught_up.next().await.unwrap();
    assert_eq!(first["type"], "snapshot", "only the snapshot, at the end");
    let mut replayed = server.stream("/api/chats/chat/stream", &identity()).await;
    let replay = replayed.until("snapshot").await;
    assert_eq!(replay.len(), 4, "{replay:?}");
    assert_eq!(replay[3]["type"], "snapshot");
    assert!(replay[3]["cursor"].as_i64().unwrap() >= last);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_and_model_selection_answer_as_karaka_does() {
    let server = start(Options::default()).await;
    server
        .post(
            "/api/chats",
            &with(json!({ "agentId": "main", "chatId": "chat" })),
        )
        .await;
    let cancelled = server.post("/api/chats/chat/cancel", &identity()).await;
    assert_eq!(
        (cancelled.status, cancelled.body.as_str()),
        (200, r#"{"accepted":true}"#)
    );
    let selected = server
        .post(
            "/api/chats/chat/model",
            &with(json!({ "provider": "mock", "model": "next", "reasoningEffort": "high" })),
        )
        .await;
    assert_eq!(
        (selected.status, selected.body.as_str()),
        (
            200,
            r#"{"selected":{"provider":"mock","model":"next","reasoningEffort":"high"}}"#
        )
    );
    let plain = server
        .post(
            "/api/chats/chat/model",
            &with(json!({ "provider": "mock", "model": "chat" })),
        )
        .await;
    assert_eq!(
        plain.body,
        r#"{"selected":{"provider":"mock","model":"chat"}}"#
    );
    let unknown = server
        .post(
            "/api/chats/chat/model",
            &with(json!({ "provider": "ghost", "model": "chat" })),
        )
        .await;
    assert_eq!(unknown.status, 500);
    // The next request goes to the selection.
    server
        .post(
            "/api/chats/chat/model",
            &with(json!({ "provider": "mock", "model": "next" })),
        )
        .await;
    server
        .model
        .replies
        .lock()
        .unwrap()
        .push_back("Done.".into());
    server
        .post(
            "/api/chats/chat/messages",
            &with(json!({ "requestId": "r", "content": [{ "type": "text", "text": "go" }] })),
        )
        .await;
    server.idle("chat").await;
    assert_eq!(server.model.requests.lock().unwrap()[0].model, "next");
    // An image is refused: there is no attachment store.
    let image = json!([{ "type": "image", "mediaType": "image/png", "data": "eA==" }]);
    let refused = server
        .post(
            "/api/chats/chat/messages",
            &with(json!({ "requestId": "image", "content": image })),
        )
        .await;
    assert_eq!(refused.status, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unavailable_default_provider_refuses_a_prompt() {
    let server = start(Options {
        default_provider: "ghost",
        ..Options::default()
    })
    .await;
    server
        .post(
            "/api/chats",
            &with(json!({ "agentId": "main", "chatId": "chat" })),
        )
        .await;
    let refused = server
        .post(
            "/api/chats/chat/messages",
            &with(json!({ "requestId": "r", "content": [{ "type": "text", "text": "hi" }] })),
        )
        .await;
    assert_eq!(
        (refused.status, refused.body.as_str()),
        (400, r#"{"code":"BAD_REQUEST","message":"Invalid request"}"#)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_chat_is_restored_and_resumed_after_a_restart() {
    let root = common::scratch();
    {
        let server = start(Options {
            root: root.clone(),
            ..Options::default()
        })
        .await;
        server
            .model
            .replies
            .lock()
            .unwrap()
            .push_back("First.".into());
        server
            .post(
                "/api/chats",
                &with(json!({ "agentId": "main", "chatId": "chat" })),
            )
            .await;
        server
            .post(
                "/api/chats/chat/messages",
                &with(json!({ "requestId": "r1", "content": [{ "type": "text", "text": "one" }] })),
            )
            .await;
        server.idle("chat").await;
        server
            .runtime
            .get::<karaka_session_persistence::DurableSessions>()
            .close()
            .await
            .unwrap();
    }
    let server = start(Options {
        root,
        ..Options::default()
    })
    .await;
    // Another user can't claim it; its owner reads it back, cold.
    let foreign = json!({ "tenantId": "tenant", "userId": "foreign" });
    let restored = server.post("/api/chats/chat/history", &identity()).await;
    assert_eq!(restored.status, 200);
    assert_eq!(
        server
            .post("/api/chats/chat/history", &foreign)
            .await
            .status,
        403
    );
    // A prompt resumes its agent, with its preset.
    server
        .model
        .replies
        .lock()
        .unwrap()
        .push_back("Second.".into());
    let receipt = server
        .post(
            "/api/chats/chat/messages",
            &with(json!({ "requestId": "r2", "content": [{ "type": "text", "text": "two" }] })),
        )
        .await;
    assert_eq!(receipt.status, 202);
    server.idle("chat").await;
    let header = server
        .runtime
        .get::<karaka_session::SessionStore>()
        .get(&SessionId::new("chat"))
        .unwrap()
        .header()
        .clone();
    assert_eq!(header.agent_preset.as_deref(), Some("main"));
    let history = server
        .post("/api/chats/chat/history", &identity())
        .await
        .json();
    assert_eq!(
        history["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == "assistant-message")
            .count(),
        2
    );
    // A duplicate from before the restart is still known.
    let duplicate = server
        .post(
            "/api/chats/chat/messages",
            &with(json!({ "requestId": "r1", "content": [{ "type": "text", "text": "one" }] })),
        )
        .await;
    assert!(
        duplicate.body.contains(r#""duplicate":true"#),
        "{}",
        duplicate.body
    );
}

fn owner() -> ApplicationOwner {
    ApplicationOwner {
        application_id: ApplicationId::new("app"),
        tenant_id: TenantId::new("tenant"),
        user_id: UserId::new("user"),
    }
}

fn question() -> Vec<AskUserQuestionItem> {
    vec![AskUserQuestionItem {
        options: Some(vec![
            AskUserQuestionOption::new("Yes"),
            AskUserQuestionOption::new("No"),
        ]),
        ..AskUserQuestionItem::new("choice", "Continue?")
    }]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_question_waits_for_the_applications_answer() {
    let server = start(Options::default()).await;
    server
        .post(
            "/api/chats",
            &with(json!({ "agentId": "main", "chatId": "chat" })),
        )
        .await;
    let agent: std::sync::Arc<dyn Agent> = server.agent("chat");
    let questions = server.runtime.get::<UserQuestions>();
    let mut events = server.stream("/api/chats/chat/stream", &identity()).await;
    assert_eq!(events.next().await.unwrap()["type"], "snapshot");
    let asking = tokio::spawn({
        let questions = std::sync::Arc::clone(&questions);
        let agent = std::sync::Arc::clone(&agent);
        async move {
            questions
                .ask(QuestionRequest {
                    agent: Some(agent),
                    ..QuestionRequest::new(question())
                })
                .await
        }
    });
    let event = events.next().await.unwrap();
    assert_eq!(event["type"], "interaction-required");
    assert_eq!(
        event["questions"],
        json!([{ "id": "choice", "question": "Continue?", "options": [{ "label": "Yes" }, { "label": "No" }] }])
    );
    assert_eq!(
        event.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["type", "cursor", "interactionId", "questions"]
    );
    let id = event["interactionId"].as_str().unwrap().to_owned();
    let answers = json!({ "answers": [{ "id": "choice", "selected": ["Yes"] }] });
    let reply = with(json!({ "interactionId": id, "answers": answers }));
    let foreign = json!({ "tenantId": "tenant", "userId": "foreign", "interactionId": id, "answers": answers });
    assert_eq!(
        server
            .post("/api/chats/chat/responses", &foreign)
            .await
            .status,
        403
    );
    assert_eq!(
        server
            .post("/api/chats/other/responses", &reply)
            .await
            .status,
        403
    );
    let accepted = server.post("/api/chats/chat/responses", &reply).await;
    assert_eq!(
        (accepted.status, accepted.body.as_str()),
        (200, r#"{"accepted":true}"#)
    );
    let answer = asking.await.unwrap().unwrap();
    assert_eq!(answer.answers[0].selected, ["Yes"]);
    let again = server.post("/api/chats/chat/responses", &reply).await;
    assert_eq!(
        (again.status, again.body.as_str()),
        (
            403,
            r#"{"code":"CHAT_FORBIDDEN","message":"Chat access is forbidden"}"#
        )
    );

    // A question asked before a stream opens is replayed to it.
    let asking = tokio::spawn({
        let questions = std::sync::Arc::clone(&questions);
        let agent = std::sync::Arc::clone(&agent);
        async move {
            questions
                .ask(QuestionRequest {
                    agent: Some(agent),
                    ..QuestionRequest::new(question())
                })
                .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut late = server.stream("/api/chats/chat/stream", &identity()).await;
    let replayed = late.until("interaction-required").await;
    let event = replayed.last().unwrap();
    let id = event["interactionId"].as_str().unwrap();
    server
        .post(
            "/api/chats/chat/responses",
            &with(json!({ "interactionId": id, "answers": { "answers": [] } })),
        )
        .await;
    assert!(asking.await.unwrap().unwrap().answers.is_empty());
    let _ = owner();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_handle_questions_a_question_goes_down_the_chain() {
    let server = start(Options {
        config: TransportConfig {
            path: "/api".into(),
            handle_questions: false,
            ..TransportConfig::default()
        },
        ..Options::default()
    })
    .await;
    server
        .post(
            "/api/chats",
            &with(json!({ "agentId": "main", "chatId": "chat" })),
        )
        .await;
    let failure = server
        .runtime
        .get::<UserQuestions>()
        .ask(QuestionRequest {
            agent: Some(server.agent("chat")),
            ..QuestionRequest::new(question())
        })
        .await
        .unwrap_err();
    assert_eq!(failure.code(), Some("NO_PROVIDER"));
    let _ = Value::Null;
}
