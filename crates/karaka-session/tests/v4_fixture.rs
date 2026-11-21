//! A dsh v4 log, as Play This Life stores it, read into Karaka and written
//! back.
//!
//! `fixtures/v4-log.json` was written by hand from the shapes in dsh v4's own
//! source and tests (`types.ts`, `message.ts`, `repair.spec.ts` and the
//! agent loop). It holds no real chat. It has one turn of two steps: a
//! system prompt, a prompt with an `rpcId`, a runtime-context snapshot, a
//! failed `assistant/attempt`, an answer with reasoning and two parallel tool
//! calls, an approval, both `tool/call`s, the two results in the order they
//! finished (one failed with `pro_required`), the final answer, and an
//! ignorable plugin record.

use karaka_model::{Block, Message, Role, ToolCall, ToolResult};
use karaka_session::{
    ContextSection, Entry, Seq, SessionEvent, SessionId, SessionLog, SessionStore, TurnEndReason,
    UserSource,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/v4-log.json");

fn entries() -> Vec<Entry> {
    serde_json::from_str(FIXTURE).expect("the fixture reads as v4 events")
}

#[test]
fn every_event_reads_into_its_karaka_variant() {
    let types: Vec<(String, &str)> = entries()
        .iter()
        .map(|entry| {
            let variant = match &entry.event {
                SessionEvent::SystemMessage { .. } => "SystemMessage",
                SessionEvent::DeveloperMessage { .. } => "DeveloperMessage",
                SessionEvent::UserMessage { .. } => "UserMessage",
                SessionEvent::AssistantMessage { .. } => "AssistantMessage",
                SessionEvent::ToolResult { .. } => "ToolResult",
                SessionEvent::TurnStart(_) => "TurnStart",
                SessionEvent::TurnEnd(_) => "TurnEnd",
                SessionEvent::StepStart(_) => "StepStart",
                SessionEvent::StepEnd(_) => "StepEnd",
                SessionEvent::ToolCall(_) => "ToolCall",
                SessionEvent::Passthrough(_) => "Passthrough",
                _ => "unknown",
            };
            (entry.event.event_type().to_owned(), variant)
        })
        .collect();
    let expected = [
        ("turn/start", "TurnStart"),
        ("step/start", "StepStart"),
        ("request/header", "Passthrough"),
        ("system/message", "SystemMessage"),
        ("user/message", "UserMessage"),
        ("user/message", "UserMessage"),
        ("assistant/attempt", "Passthrough"),
        ("assistant/message", "AssistantMessage"),
        ("approval/asked", "Passthrough"),
        ("approval/decided", "Passthrough"),
        ("tool/call", "ToolCall"),
        ("tool/call", "ToolCall"),
        ("tool/result", "ToolResult"),
        ("tool/result", "ToolResult"),
        ("step/end", "StepEnd"),
        ("step/start", "StepStart"),
        ("assistant/message", "AssistantMessage"),
        ("step/end", "StepEnd"),
        ("plugin:test/marker", "Passthrough"),
        ("turn/end", "TurnEnd"),
    ];
    let expected: Vec<(String, &str)> = expected
        .iter()
        .map(|(event_type, variant)| ((*event_type).to_owned(), *variant))
        .collect();
    assert_eq!(types, expected);
}

#[test]
fn the_log_writes_back_to_the_same_json() {
    let original: Value = serde_json::from_str(FIXTURE).unwrap();
    let written = serde_json::to_value(entries()).unwrap();
    // Compared as JSON values: the same keys, values and array order. Key
    // order inside the typed events may differ; see the crate docs.
    assert_eq!(written, original);
}

#[test]
fn every_seq_and_time_is_kept() {
    let original: Value = serde_json::from_str(FIXTURE).unwrap();
    let log = SessionLog::restore(entries()).expect("the fixture is a valid log");
    assert_eq!(log.len(), 20);
    for (index, (entry, raw)) in log
        .events()
        .iter()
        .zip(original.as_array().unwrap())
        .enumerate()
    {
        assert_eq!(entry.seq, Seq(index as u64));
        assert_eq!(Some(entry.time.0), raw["time"].as_i64());
    }
    assert!(log.events()[18].ignorable);
    assert_eq!(log.next_seq(), Seq(20));
}

#[test]
fn the_fields_play_this_life_reads_are_typed() {
    let entries = entries();

    let SessionEvent::UserMessage { message, .. } = &entries[4].event else {
        panic!("seq 4 is the prompt");
    };
    assert_eq!(message.id, "msg-user-1");
    assert_eq!(message.source.rpc_id(), Some("rpc-1"));

    let SessionEvent::UserMessage { message, .. } = &entries[5].event else {
        panic!("seq 5 is the runtime context");
    };
    let UserSource::RuntimeContext(context) = &message.source else {
        panic!("seq 5 comes from the runtime context");
    };
    assert_eq!(context.form.as_deref(), Some("snapshot"));
    assert_eq!(
        context.sections.as_deref().map(|sections| sections
            .iter()
            .map(|ContextSection { name, .. }| name.as_str())
            .collect::<Vec<_>>()),
        Some(vec!["clock"])
    );

    let SessionEvent::AssistantMessage { data, .. } = &entries[7].event else {
        panic!("seq 7 is the first answer");
    };
    assert_eq!((data.turn, data.step), (1, 1));
    let usage = data.usage.as_ref().expect("the answer has usage");
    assert_eq!((usage.input_tokens, usage.output_tokens), (40, 12));
    assert_eq!(data.stream.len(), 4);

    let SessionEvent::ToolCall(call) = &entries[10].event else {
        panic!("seq 10 starts call-a");
    };
    assert_eq!(
        (call.turn, call.call_id.as_str(), call.name.as_str()),
        (1, "call-a", "clock")
    );

    let SessionEvent::ToolResult { data, surface } = &entries[13].event else {
        panic!("seq 13 is call-a's result");
    };
    assert_eq!(surface.sources, [Seq(10)]);
    let error = data.error.as_ref().expect("call-a failed");
    assert_eq!(error.code, "pro_required");
    assert_eq!(error.reason.as_deref(), Some("The pro plan is required."));

    let SessionEvent::TurnEnd(end) = &entries[19].event else {
        panic!("seq 19 ends the turn");
    };
    assert_eq!(end.reason, TurnEndReason::Completed);

    let SessionEvent::Passthrough(approval) = &entries[8].event else {
        panic!("seq 8 is the approval");
    };
    assert_eq!(approval.data["callId"], "call-a");
}

#[test]
fn the_log_derives_the_expected_history() {
    let log = SessionLog::restore(entries()).unwrap();
    let history = log.history();
    assert_eq!(history.system.as_deref(), Some("You are a test assistant."));

    let call = |id: &str, zone: &str| {
        Block::ToolCall(ToolCall {
            id: id.into(),
            name: "clock".into(),
            arguments: format!("{{\"zone\":\"{zone}\"}}"),
        })
    };
    assert_eq!(
        history.messages,
        [
            Message::user("What time is it in two places?"),
            Message::user("Runtime context: fake clock reads 12:00."),
            Message {
                role: Role::Assistant,
                content: vec![
                    Block::Reasoning {
                        text: "Two zones, two calls.".into(),
                        replay: Some("sig-fixture-1".into()),
                    },
                    Block::Text("Let me check both.".into()),
                    call("call-a", "UTC"),
                    call("call-b", "UTC+6"),
                ],
            },
            // Both results in one message, in the order they finished.
            Message {
                role: Role::User,
                content: vec![
                    Block::ToolResult(ToolResult {
                        call_id: "call-b".into(),
                        content: "18:00".into(),
                        is_error: false,
                    }),
                    Block::ToolResult(ToolResult {
                        call_id: "call-a".into(),
                        content: "Error: this tool needs the pro plan.".into(),
                        is_error: true,
                    }),
                ],
            },
            Message {
                role: Role::Assistant,
                content: vec![Block::Text(
                    "It is 18:00 in UTC+6. The other clock needs the pro plan.".into()
                )],
            },
        ]
    );
}

#[test]
fn a_closed_log_needs_no_repair() {
    let log = SessionLog::restore(entries()).unwrap();
    assert_eq!(log.repair_closers(), []);
}

#[test]
fn the_log_cut_after_its_tool_calls_repairs_to_two_failed_results() {
    // As if the process died after both tools started, before either
    // result was logged.
    let cut: Vec<Entry> = entries().into_iter().take(12).collect();
    let store = SessionStore::new();
    let session = store.restore(SessionId::new("cut"), cut).unwrap();

    let closers = session.repair().unwrap();
    let types: Vec<&str> = closers
        .iter()
        .map(|entry| entry.event.event_type())
        .collect();
    assert_eq!(
        types,
        ["tool/result", "tool/result", "step/end", "turn/end"]
    );
    assert_eq!(
        closers.iter().map(|entry| entry.seq.0).collect::<Vec<_>>(),
        [12, 13, 14, 15]
    );

    // The repaired log derives a history a provider accepts: each call has
    // one result, right after it.
    let messages = session.messages();
    let results = messages.last().expect("the results' message");
    assert_eq!(results.role, Role::User);
    let ids: Vec<(&str, bool)> = results
        .content
        .iter()
        .map(|block| match block {
            Block::ToolResult(result) => (result.call_id.as_str(), result.is_error),
            other => panic!("expected only results, got {other:?}"),
        })
        .collect();
    assert_eq!(ids, [("call-a", true), ("call-b", true)]);
    assert_eq!(session.repair(), Ok(Vec::new()));
}
