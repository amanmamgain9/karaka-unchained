//! What a session log admits, beyond seqs and surface operations, held
//! against dsh v4's session: surface fields on events that can't join the
//! surface, `image/offload`, and developer messages with their tool
//! changes. (dsh: `validateSurfaceMetadata`, `validateSessionEventData`,
//! `assertDeveloperHeader` and `planSurfaceEvent` in
//! `core/session/src/surface.ts`, and `imageOffloadProjection` in
//! `compaction/compaction-image-offload/src/projection.ts`, which dsh's
//! stored-log reader always installs.)

use karaka_session::{Entry, SessionLog};
use serde_json::{Value, json};

/// Reads `rows` as events and restores them as one log, giving the refusal.
fn admit(rows: Value) -> Result<SessionLog, String> {
    let entries: Vec<Entry> = serde_json::from_value(rows).map_err(|error| error.to_string())?;
    SessionLog::restore(entries).map_err(|error| error.to_string())
}

fn refusal(rows: Value) -> String {
    match admit(rows) {
        Ok(_) => panic!("the log was admitted"),
        Err(message) => message,
    }
}

fn start() -> Value {
    json!({ "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 } })
}

fn user(seq: u64, content: Value) -> Value {
    json!({ "type": "user/message", "seq": seq, "time": 2, "surfaceOp": "append",
            "data": { "id": format!("u{seq}"), "role": "user", "content": content, "source": { "kind": "user" } } })
}

fn image(name: &str) -> Value {
    json!({ "type": "image", "attachment": { "attachmentId": name, "mediaType": "image/png",
                                             "bytes": 3, "width": 1, "height": 1 } })
}

// --- surface fields (`surfaceOpOf`) ---

#[test]
fn a_known_event_that_cant_join_the_surface_cant_carry_surface_fields() {
    let asked = |extra: Value| {
        let mut row = json!({ "type": "approval/asked", "seq": 1, "time": 2, "data": {} });
        row.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!([start(), row])
    };
    assert!(refusal(asked(json!({ "surfaceOp": "append" }))).contains(
        "session event \"approval/asked\" is not surface-eligible and cannot carry surfaceOp"
    ));
    assert!(refusal(asked(json!({ "sourceEventSeqs": [0] }))).contains(
        "session event \"approval/asked\" is not surface-eligible and cannot carry sourceEventSeqs"
    ));
    // An unknown type the writer marked ignorable keeps whatever it has.
    let opaque = json!({ "type": "future/note", "seq": 1, "time": 2, "data": {}, "ignorable": true,
                         "surfaceOp": { "opaque": true }, "sourceEventSeqs": "anything" });
    assert!(admit(json!([start(), opaque])).is_ok());
    // An unknown type that isn't ignorable may not.
    let required =
        json!({ "type": "future/note", "seq": 1, "time": 2, "data": {}, "surfaceOp": "append" });
    assert!(refusal(json!([start(), required])).contains("cannot carry surfaceOp"));
}

#[test]
fn a_lifecycle_event_names_the_surface_field_it_cant_carry() {
    let with = |key: &str, value: Value| {
        let mut row = start();
        row[key] = value;
        json!([row])
    };
    assert!(refusal(with("surfaceOp", json!("append"))).contains(
        "session event \"turn/start\" is not surface-eligible and cannot carry surfaceOp"
    ));
    assert!(refusal(with("sourceEventSeqs", json!([0]))).contains(
        "session event \"turn/start\" is not surface-eligible and cannot carry sourceEventSeqs"
    ));
}

// --- image/offload ---

fn offload(seq: u64, targets: Value) -> Value {
    json!({ "type": "image/offload", "seq": seq, "time": 3, "data": { "targets": targets } })
}

#[test]
fn an_offload_of_images_on_the_surface_is_admitted() {
    let rows = json!([
        start(),
        user(
            1,
            json!([image("a"), { "type": "text", "text": "and" }, image("b")])
        ),
        offload(2, json!([{ "seq": 1, "imageIndexes": [1] }])),
        offload(3, json!([{ "seq": 1, "imageIndexes": [0] }])),
    ]);
    assert!(admit(rows).is_ok());
}

#[test]
fn a_malformed_offload_is_refused_in_dshs_words() {
    let images = user(1, json!([image("a"), image("b")]));
    for (targets, expected) in [
        (
            json!([]),
            "image/offload: data must contain a nonempty targets array",
        ),
        (
            json!([{ "seq": 1 }]),
            "image/offload: each target must contain a seq and nonempty imageIndexes",
        ),
        (
            json!([{ "seq": 1, "imageIndexes": [0] }, { "seq": 1, "imageIndexes": [1] }]),
            "image/offload: duplicate target seq 1",
        ),
        (
            json!([{ "seq": 0, "imageIndexes": [0] }]),
            "image/offload: target seq 0 is not a current surface node",
        ),
        (
            json!([{ "seq": 1, "imageIndexes": [1, 0] }]),
            "image/offload: imageIndexes must be strictly increasing non-negative safe integers",
        ),
        (
            json!([{ "seq": 1, "imageIndexes": [0, 2] }]),
            "image/offload: image index 2 does not exist",
        ),
    ] {
        let message = refusal(json!([start(), images.clone(), offload(2, targets)]));
        assert!(message.contains(expected), "{expected}: {message}");
    }
    // Offloading the same image twice.
    let message = refusal(json!([
        start(),
        images,
        offload(2, json!([{ "seq": 1, "imageIndexes": [0] }])),
        offload(3, json!([{ "seq": 1, "imageIndexes": [0] }])),
    ]));
    assert!(
        message.contains("image/offload: image index 0 is already offloaded"),
        "{message}"
    );
    // A surface node that isn't a user message or a tool result.
    let system = json!({ "type": "system/message", "seq": 1, "time": 2, "surfaceOp": "append",
        "data": { "turn": 1, "step": 1, "message": { "id": "s", "role": "system", "content": [],
                                                     "source": { "kind": "system-prompt" } } } });
    let message = refusal(json!([
        start(),
        system,
        offload(2, json!([{ "seq": 1, "imageIndexes": [0] }])),
    ]));
    assert!(
        message.contains("image/offload: target seq 1 must be user/message or tool/result"),
        "{message}"
    );
}

// --- developer messages (`validateSessionEventData`, `assertDeveloperHeader`) ---

fn header(seq: u64, tools: Value) -> Value {
    json!({ "type": "request/header", "seq": seq, "time": 2, "data": {
        "header": { "config": { "provider": "mock", "model": "m" }, "tools": tools },
        "reason": "initial"
    } })
}

fn clock() -> Value {
    json!({ "name": "clock", "description": "Reads a clock.", "parameters": { "type": "object" } })
}

fn developer(seq: u64, content: Value, header_seq: Option<u64>) -> Value {
    let mut data = json!({ "turn": 1, "step": 1, "message": {
        "id": format!("d{seq}"), "role": "developer", "content": content, "source": { "kind": "tools" }
    } });
    if let Some(header_seq) = header_seq {
        data["headerSeq"] = header_seq.into();
    }
    json!({ "type": "developer/message", "seq": seq, "time": 3, "surfaceOp": "append", "data": data })
}

fn addition(name: &str) -> Value {
    json!({ "type": "tool-addition", "toolName": name })
}

#[test]
fn a_tool_addition_naming_its_tool_in_an_earlier_header_is_admitted() {
    let rows = json!([
        start(),
        header(1, json!([clock()])),
        developer(
            2,
            json!([addition("clock"), { "type": "tool-removal", "toolName": "old" }]),
            Some(1)
        ),
    ]);
    assert!(admit(rows).is_ok());
}

#[test]
fn tool_changes_belong_to_developer_messages_only() {
    let message = refusal(json!([start(), user(1, json!([addition("clock")]))]));
    assert!(
        message.contains("tool-change blocks require developer role"),
        "{message}"
    );
}

#[test]
fn a_developer_role_and_a_developer_event_go_together() {
    let mut row = user(1, json!([]));
    row["data"]["role"] = "developer".into();
    let message = refusal(json!([start(), row]));
    assert!(
        message.contains("developer/message and developer role must occur together"),
        "{message}"
    );
}

#[test]
fn a_malformed_tool_change_is_refused_in_dshs_words() {
    let base = || vec![start(), header(1, json!([clock()]))];
    let cases = [
        (
            developer(2, json!([addition("")]), Some(1)),
            "tool-addition requires a nonempty toolName",
        ),
        (
            developer(2, json!([{ "type": "tool-removal", "toolName": "" }]), None),
            "tool-removal requires a nonempty toolName",
        ),
        (
            developer(
                2,
                json!([{ "type": "tool-addition", "toolName": "clock", "tool": clock() }]),
                Some(1),
            ),
            "tool-addition must omit inline tool definitions",
        ),
        (
            developer(2, json!([addition("clock")]), None),
            "requires headerSeq exactly when tool additions are present",
        ),
        (
            developer(2, json!([{ "type": "text", "text": "hi" }]), Some(1)),
            "requires headerSeq exactly when tool additions are present",
        ),
        (
            developer(2, json!([addition("clock")]), Some(0)),
            "developer/message headerSeq must reference an earlier request/header",
        ),
        (
            developer(2, json!([addition("missing")]), Some(1)),
            "developer/message tool-addition \"missing\" must name exactly one tool in headerSeq 1",
        ),
    ];
    for (row, expected) in cases {
        let mut rows = base();
        rows.push(row);
        let message = refusal(Value::Array(rows));
        assert!(message.contains(expected), "{expected}: {message}");
    }

    // A tool named twice, or defined incompletely, in the header.
    let twice = json!([
        start(),
        header(1, json!([clock(), clock()])),
        developer(2, json!([addition("clock")]), Some(1))
    ]);
    assert!(refusal(twice).contains(
        "developer/message tool-addition \"clock\" must name exactly one tool in headerSeq 1"
    ));
    let bare = json!([
        start(),
        header(1, json!([{ "name": "clock" }])),
        developer(2, json!([addition("clock")]), Some(1))
    ]);
    assert!(refusal(bare).contains("developer/message tool-addition \"clock\" requires a complete tool definition in headerSeq 1"));
    let deferred = {
        let mut tool = clock();
        tool["deferLoading"] = false.into();
        json!([
            start(),
            header(1, json!([tool])),
            developer(2, json!([addition("clock")]), Some(1))
        ])
    };
    assert!(
        refusal(deferred)
            .contains("developer/message referenced tool deferLoading must be true when present")
    );
}
