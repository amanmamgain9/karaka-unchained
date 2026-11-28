//! The error table, the request schemas and the application state.
//! (Karaka: `routes.spec.ts`'s exposure table, the sdk's schemas, and
//! `application-state.spec.ts`.)

use karaka_agent::ModelSelection;
use karaka_session::{Entry, Passthrough, Seq, SessionEvent, Time};
use karaka_transport_http::{
    AddressRequest, AppError, ApplicationState, Content, ModelRequest, Prompt, Respond,
    application_state, expose,
};
use serde_json::json;

#[test]
fn each_internal_code_is_exposed_without_its_message() {
    for (code, status, exposed) in [
        ("forbidden", 403, "CHAT_FORBIDDEN"),
        ("unavailable", 503, "CHAT_UNAVAILABLE"),
        ("CHAT_FORBIDDEN", 403, "CHAT_FORBIDDEN"),
        ("SESSION_QUERY_SESSION_NOT_FOUND", 404, "CHAT_NOT_FOUND"),
        ("session/not-found", 404, "CHAT_NOT_FOUND"),
        ("agent-preset/not-found", 404, "AGENT_NOT_FOUND"),
        ("session/conflict", 409, "CHAT_CONFLICT"),
        ("agent-preset/conflict", 409, "CHAT_CONFLICT"),
        ("session/agent-busy", 409, "CHAT_CONFLICT"),
        ("BAD_REQUEST", 400, "BAD_REQUEST"),
        ("gateway/bad-request", 400, "BAD_REQUEST"),
        ("session/model-unavailable", 400, "BAD_REQUEST"),
        ("session/attachment-invalid", 400, "BAD_REQUEST"),
        ("agent-preset/invalid", 400, "BAD_REQUEST"),
        ("unexpected", 500, "INTERNAL_ERROR"),
    ] {
        let error = AppError::coded(code, "private credential");
        let shown = expose(&error);
        assert_eq!((shown.status, shown.code), (status, exposed), "{code}");
        assert!(!shown.message.contains("private"));
    }
    assert_eq!(
        expose(&AppError::internal("private")).code,
        "INTERNAL_ERROR"
    );
}

#[test]
fn the_schemas_take_what_the_sdk_takes() {
    let prompt = Prompt::parse(&json!({
        "tenantId": "t", "userId": "u", "requestId": "r",
        "content": [
            { "type": "text", "text": "hello" },
            { "type": "image", "mediaType": "image/png", "data": "eA==", "name": "image.png" },
        ],
    }))
    .unwrap();
    assert_eq!(prompt.content[0], Content::Text("hello".into()));
    for bad in [
        json!({ "tenantId": "t", "userId": "u", "requestId": "r", "content": [] }),
        json!({ "tenantId": "t", "userId": "u", "requestId": "r", "content": [{ "type": "text", "text": "" }] }),
        json!({ "tenantId": "t", "userId": "u", "requestId": "r", "content": [{ "type": "image", "mediaType": "image/bmp", "data": "x" }] }),
        json!({ "tenantId": "t", "userId": "u", "requestId": "r", "content": [{ "type": "audio" }] }),
        json!({ "tenantId": "", "userId": "u", "requestId": "r", "content": [{ "type": "text", "text": "x" }] }),
    ] {
        assert!(Prompt::parse(&bad).is_err(), "{bad}");
    }
    assert_eq!(
        AddressRequest::parse(&json!({ "tenantId": "t", "userId": "u", "cursor": -1 }))
            .unwrap()
            .cursor,
        Some(-1)
    );
    for bad in [
        json!(-2),
        json!(1.5),
        json!("1"),
        json!(null),
        json!(9_007_199_254_740_992_u64),
    ] {
        assert!(
            AddressRequest::parse(&json!({ "tenantId": "t", "userId": "u", "cursor": bad }))
                .is_err(),
            "{bad}"
        );
    }
    let model = ModelRequest::parse(
        &json!({ "tenantId": "t", "userId": "u", "provider": "p", "model": "m" }),
    )
    .unwrap();
    assert_eq!(model.reasoning_effort, None);
    assert!(ModelRequest::parse(&json!({ "tenantId": "t", "userId": "u", "provider": "p", "model": "m", "reasoningEffort": "" })).is_err());
    let respond = Respond::parse(&json!({
        "tenantId": "t", "userId": "u", "interactionId": "i",
        "answers": { "answers": [{ "id": "q", "selected": ["a"], "custom": "c" }] },
    }))
    .unwrap();
    assert_eq!(respond.answers[0].custom.as_deref(), Some("c"));
    assert!(
        Respond::parse(&json!({
            "tenantId": "t", "userId": "u", "interactionId": "i",
            "answers": { "answers": [{ "id": "q", "selected": [1] }] },
        }))
        .is_err()
    );
}

fn entry(seq: u64, event_type: &str, data: serde_json::Value) -> Entry {
    Entry {
        seq: Seq(seq),
        time: Time(1),
        ignorable: false,
        event: SessionEvent::Passthrough(Passthrough {
            event_type: event_type.into(),
            data,
            surface_op: None,
            source_event_seqs: None,
        }),
    }
}

fn selection(model: &str, effort: Option<&str>) -> ModelSelection {
    ModelSelection {
        provider: "mock".into(),
        model: model.into(),
        reasoning_effort: effort.map(str::to_owned),
    }
}

#[test]
fn the_state_keeps_receipts_once_and_consumes_a_matching_selection() {
    let inbox = entry(
        0,
        "agent/inbox/spliced",
        json!({ "inserted": [
            { "source": { "kind": "user", "rpcId": "a" } },
            { "source": { "kind": "user", "rpcId": "a" } },
            { "source": { "kind": "user" } },
            { "source": { "kind": "fixture", "rpcId": "x" } },
        ] }),
    );
    let selected = entry(
        1,
        "model/selection",
        json!({ "provider": "mock", "model": "next", "reasoningEffort": "high" }),
    );
    let state = application_state(&[inbox.clone(), selected.clone()]);
    assert_eq!(state.request_ids, ["a"]);
    assert_eq!(state.pending, Some(selection("next", Some("high"))));
    let header = entry(
        2,
        "request/header",
        json!({ "header": { "config": { "provider": "mock", "model": "next", "reasoningEffort": "high" } } }),
    );
    let state = application_state(&[inbox.clone(), selected.clone(), header]);
    assert_eq!(state.pending, None);
    assert_eq!(state.last_used, Some(selection("next", Some("high"))));
    // An adapter-default effort isn't the last used one's.
    let defaulted = entry(
        2,
        "request/header",
        json!({ "header": { "config": { "provider": "mock", "model": "chat", "reasoningEffort": "high" }, "adapterDefaults": { "reasoningEffort": true } } }),
    );
    let state = application_state(&[selected, defaulted]);
    assert_eq!(state.last_used, Some(selection("chat", None)));
    assert!(state.pending.is_some());
    assert_eq!(application_state(&[]), ApplicationState::default());
}
