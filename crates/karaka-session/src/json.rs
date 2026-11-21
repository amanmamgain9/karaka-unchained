//! An [`Entry`] as JSON: dsh v4's event envelope.
//! (dsh v4: `SessionEvent` in `types.ts`, checked by
//! `assertSessionEventEnvelope` in `index.ts` and `surfaceOpOf` in
//! `surface.ts`.)
//!
//! The envelope has exactly these keys, and v4 rejects any other:
//!
//! ```json
//! { "type": "tool/result", "seq": 12, "time": 1700000000000, "data": { ... },
//!   "surfaceOp": "append", "sourceEventSeqs": [11], "ignorable": true }
//! ```
//!
//! The last three are optional. `surfaceOp` is required on the five surface
//! types and forbidden on the lifecycle types Karaka models, which include
//! `request/header`, `request/context` and `assistant/attempt`. Other types are
//! passthrough and keep whatever they carry.
//!
//! Only the shape of one event is checked here. Whether it fits the log,
//! such as its seq being next or a replacement naming real surface nodes,
//! is checked when it joins a log (see `surface.rs`).

use serde::de::{DeserializeOwned, Error as _};
use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

use crate::data::{is_false, only_true};
use crate::event::{Entry, Passthrough, Seq, SessionEvent, SurfaceIntent, SurfaceOp, Time};

/// The envelope with its `data` still raw.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Envelope {
    #[serde(rename = "type")]
    event_type: String,
    seq: Seq,
    time: Time,
    data: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    surface_op: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_event_seqs: Option<Value>,
    #[serde(
        default,
        skip_serializing_if = "is_false",
        deserialize_with = "only_true"
    )]
    ignorable: bool,
}

impl Serialize for Entry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        envelope_of(self)
            .map_err(S::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let envelope = Envelope::deserialize(deserializer)?;
        let subject = format!("{} at seq {}", envelope.event_type, envelope.seq);
        entry_of(envelope).map_err(|reason| D::Error::custom(format!("{subject}: {reason}")))
    }
}

fn envelope_of(entry: &Entry) -> Result<Envelope, serde_json::Error> {
    let (data, surface_op, source_event_seqs) = match &entry.event {
        SessionEvent::SystemMessage { data, surface } => (
            to_json(data)?,
            Some(op_json(surface.op)),
            sources_json(surface),
        ),
        SessionEvent::DeveloperMessage { data, surface } => (
            to_json(data)?,
            Some(op_json(surface.op)),
            sources_json(surface),
        ),
        SessionEvent::UserMessage { message, surface } => (
            to_json(message)?,
            Some(op_json(surface.op)),
            sources_json(surface),
        ),
        SessionEvent::AssistantMessage { data, op } => (to_json(data)?, Some(op_json(*op)), None),
        SessionEvent::ToolResult { data, surface } => (
            to_json(data)?,
            Some(op_json(surface.op)),
            sources_json(surface),
        ),
        SessionEvent::TurnStart(data) => (to_json(data)?, None, None),
        SessionEvent::TurnEnd(data) => (to_json(data)?, None, None),
        SessionEvent::StepStart(data) | SessionEvent::StepEnd(data) => (to_json(data)?, None, None),
        SessionEvent::ToolCall(data) => (to_json(data)?, None, None),
        SessionEvent::RequestHeader(data) => (to_json(data)?, None, None),
        SessionEvent::RequestContext(data) => (to_json(data)?, None, None),
        SessionEvent::AssistantAttempt(data) => (to_json(data)?, None, None),
        SessionEvent::Passthrough(passthrough) => (
            passthrough.data.clone(),
            passthrough.surface_op.clone(),
            passthrough.source_event_seqs.clone(),
        ),
    };
    Ok(Envelope {
        event_type: entry.event.event_type().to_owned(),
        seq: entry.seq,
        time: entry.time,
        data,
        surface_op,
        source_event_seqs,
        ignorable: entry.ignorable,
    })
}

fn entry_of(envelope: Envelope) -> Result<Entry, String> {
    let Envelope {
        event_type,
        seq,
        time,
        data,
        surface_op,
        source_event_seqs,
        ignorable,
    } = envelope;
    let event = match event_type.as_str() {
        "system/message" => SessionEvent::SystemMessage {
            data: from_json(data)?,
            surface: intent_of(&event_type, surface_op, source_event_seqs)?,
        },
        "developer/message" => SessionEvent::DeveloperMessage {
            data: from_json(data)?,
            surface: intent_of(&event_type, surface_op, source_event_seqs)?,
        },
        "user/message" => SessionEvent::UserMessage {
            message: from_json(data)?,
            surface: intent_of(&event_type, surface_op, source_event_seqs)?,
        },
        "assistant/message" => {
            if source_event_seqs.is_some() {
                return Err(
                    "assistant/message embeds its source stream and cannot carry \
                     sourceEventSeqs"
                        .into(),
                );
            }
            SessionEvent::AssistantMessage {
                data: from_json(data)?,
                op: required_op(&event_type, surface_op)?,
            }
        }
        "tool/result" => SessionEvent::ToolResult {
            data: from_json(data)?,
            surface: intent_of(&event_type, surface_op, source_event_seqs)?,
        },
        "turn/start" | "turn/end" | "step/start" | "step/end" | "tool/call" | "request/header"
        | "request/context" | "assistant/attempt" => {
            if surface_op.is_some() || source_event_seqs.is_some() {
                return Err(format!(
                    "session event \"{event_type}\" is not surface-eligible and cannot \
                     carry surfaceOp or sourceEventSeqs"
                ));
            }
            match event_type.as_str() {
                "turn/start" => SessionEvent::TurnStart(from_json(data)?),
                "turn/end" => SessionEvent::TurnEnd(from_json(data)?),
                "step/start" => SessionEvent::StepStart(from_json(data)?),
                "step/end" => SessionEvent::StepEnd(from_json(data)?),
                "tool/call" => SessionEvent::ToolCall(from_json(data)?),
                "request/header" => SessionEvent::RequestHeader(from_json(data)?),
                "request/context" => SessionEvent::RequestContext(from_json(data)?),
                _ => SessionEvent::AssistantAttempt(from_json(data)?),
            }
        }
        _ => SessionEvent::Passthrough(Passthrough {
            event_type,
            data,
            surface_op,
            source_event_seqs,
        }),
    };
    Ok(Entry {
        seq,
        time,
        ignorable,
        event,
    })
}

fn intent_of(
    event_type: &str,
    surface_op: Option<Value>,
    source_event_seqs: Option<Value>,
) -> Result<SurfaceIntent, String> {
    let op = required_op(event_type, surface_op)?;
    let sources = match source_event_seqs {
        None => Vec::new(),
        Some(value) => {
            let sources: Vec<Seq> = serde_json::from_value(value).map_err(|_| {
                format!(
                    "session event \"{event_type}\" sourceEventSeqs must densely contain \
                     non-negative safe integers"
                )
            })?;
            // An empty list would read back as "no sources" and be dropped
            // on write, so it is refused, as v4 refuses it.
            if sources.is_empty() {
                return Err("sourceEventSeqs must not be empty".into());
            }
            sources
        }
    };
    Ok(SurfaceIntent { op, sources })
}

fn required_op(event_type: &str, surface_op: Option<Value>) -> Result<SurfaceOp, String> {
    let Some(value) = surface_op else {
        return Err(format!(
            "session event \"{event_type}\" is surface-eligible and requires a surfaceOp marker"
        ));
    };
    if value == "append" {
        return Ok(SurfaceOp::Append);
    }
    // dsh v4 `isReplaceOp`: exactly the three keys, nothing else.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields, rename_all = "camelCase")]
    struct Replace {
        op: String,
        start_seq: Seq,
        end_seq: Seq,
    }
    match serde_json::from_value::<Replace>(value) {
        Ok(replace) if replace.op == "replace" => Ok(SurfaceOp::Replace {
            start: replace.start_seq,
            end: replace.end_seq,
        }),
        _ => Err(format!(
            "session event \"{event_type}\" carries an invalid replace surfaceOp"
        )),
    }
}

fn op_json(op: SurfaceOp) -> Value {
    match op {
        SurfaceOp::Append => json!("append"),
        SurfaceOp::Replace { start, end } => {
            json!({ "op": "replace", "startSeq": start, "endSeq": end })
        }
    }
}

fn sources_json(surface: &SurfaceIntent) -> Option<Value> {
    (!surface.sources.is_empty()).then(|| json!(surface.sources))
}

fn to_json(value: &impl Serialize) -> Result<Value, serde_json::Error> {
    serde_json::to_value(value)
}

fn from_json<T: DeserializeOwned>(data: Value) -> Result<T, String> {
    serde_json::from_value(data).map_err(|error| error.to_string())
}

/// Ported from dsh v4 `packages/core/session/tests/canonical-envelopes.spec.ts`
/// and the envelope cases of `session.spec.ts`.
#[cfg(test)]
mod tests {
    use karaka_model::ToolResult;
    use serde_json::{Value, json};

    use crate::{
        CancelCause, Entry, Failure, SessionEvent, SessionLog, ToolResultData, TurnEndReason,
    };

    fn read(value: &Value) -> Result<Entry, String> {
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())
    }

    /// Reads `value` and writes it back, and checks nothing changed.
    fn round_trip(value: Value) -> Entry {
        let entry = read(&value).expect("the event reads");
        assert_eq!(serde_json::to_value(&entry).unwrap(), value);
        entry
    }

    fn refusal(value: Value) -> String {
        read(&value).expect_err("the event is refused")
    }

    fn system_message(source: Value) -> Value {
        json!({
            "type": "system/message", "seq": 0, "time": 1, "surfaceOp": "append",
            "data": { "turn": 1, "step": 1, "message": {
                "role": "system", "content": [{ "type": "text", "text": "prompt" }],
                "source": source, "id": "m-1",
            } },
        })
    }

    fn user_message() -> Value {
        json!({
            "type": "user/message", "seq": 0, "time": 1, "surfaceOp": "append",
            "data": { "content": [], "source": { "kind": "user" }, "role": "user", "id": "m-1" },
        })
    }

    // "preserves nested header, source, and data extras without normalizing
    // accepted events"
    #[test]
    fn extra_fields_at_every_level_are_kept() {
        let mut system =
            system_message(json!({ "kind": "system-prompt", "extra": { "nested": true } }));
        system["data"]["extra"] = json!(["retained"]);
        system["data"]["message"]["content"][0]["cacheHint"] = json!("keep");
        system["data"]["message"]["note"] = json!(null);
        let entry = round_trip(system);
        assert!(matches!(entry.event, SessionEvent::SystemMessage { .. }));

        let header = json!({
            "type": "request/header", "seq": 0, "time": 1,
            "data": {
                "header": { "config": { "provider": "mock", "model": "mock", "maxTokens": 1 },
                            "extra": { "nested": [true, null] } },
                "reason": "initial", "extra": ["retained"],
            },
        });
        let SessionEvent::RequestHeader(data) = round_trip(header).event else {
            panic!("a request/header is typed");
        };
        assert_eq!(data.extra["extra"], json!(["retained"]));
        assert_eq!(
            data.header.extra["extra"],
            json!({ "nested": [true, null] })
        );
    }

    #[test]
    fn a_passthrough_event_writes_back_with_its_keys_in_the_same_order() {
        let text = r#"{"type":"approval/asked","seq":0,"time":1,"data":{"toolName":"clock","id":"a-1","args":{"zone":"UTC","at":1}}}"#;
        let entry: Entry = serde_json::from_str(text).unwrap();
        assert!(matches!(entry.event, SessionEvent::Passthrough(_)));
        assert_eq!(serde_json::to_string(&entry).unwrap(), text);
    }

    /// dsh compares tools by their JSON text, so each tool in a header
    /// keeps its keys in the order they were read. The header's own fields
    /// come out in dsh's order.
    #[test]
    fn a_headers_tools_keep_their_key_order() {
        let text = r#"{"type":"request/header","seq":0,"time":1,"data":{"reason":"initial","header":{"tools":[{"name":"clock","parameters":{"type":"object","properties":{"zone":{"type":"string"}}},"description":""}],"config":{"provider":"mock","model":"mock"}}}}"#;
        let entry: Entry = serde_json::from_str(text).unwrap();
        assert!(matches!(entry.event, SessionEvent::RequestHeader(_)));
        assert_eq!(
            serde_json::to_string(&entry).unwrap(),
            concat!(
                r#"{"type":"request/header","seq":0,"time":1,"data":{"header":{"config":"#,
                r#"{"provider":"mock","model":"mock"},"tools":[{"name":"clock","parameters":"#,
                r#"{"type":"object","properties":{"zone":{"type":"string"}}},"description":""}]},"#,
                r#""reason":"initial"}}"#
            )
        );
    }

    #[test]
    fn the_three_request_events_read_as_typed_and_write_back() {
        let header = round_trip(json!({
            "type": "request/header", "seq": 0, "time": 1,
            "data": {
                "header": {
                    "config": { "provider": "anthropic", "model": "claude-opus-5-5",
                                "reasoningEffort": "high", "maxTokens": 256000, "temperature": 1 },
                    "adapterDefaults": { "maxTokens": true },
                },
                "reason": "change", "startsSeries": true,
            },
        }));
        let SessionEvent::RequestHeader(data) = header.event else {
            panic!("a request/header is typed");
        };
        assert_eq!(data.reason, crate::RequestHeaderReason::Change);
        assert!(data.starts_series);
        assert_eq!(data.header.config.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(data.header.config.max_tokens, Some(256_000));

        let context = round_trip(json!({
            "type": "request/context", "seq": 0, "time": 1,
            "data": { "provider": "anthropic", "model": "claude-opus-5-5",
                      "contextWindow": 200000, "systemPromptUpdate": "in-history", "note": 1 },
        }));
        let SessionEvent::RequestContext(context) = context.event else {
            panic!("a request/context is typed");
        };
        assert_eq!(context.context_window, Some(200_000));
        assert_eq!(
            context.system_prompt_update.as_deref(),
            Some(crate::IN_HISTORY)
        );
        assert_eq!(context.extra["note"], json!(1));

        let attempt = round_trip(json!({
            "type": "assistant/attempt", "seq": 0, "time": 1,
            "data": { "turn": 1, "step": 2, "stream": [{ "type": "text-chunks", "texts": ["x"] }] },
        }));
        assert!(matches!(attempt.event, SessionEvent::AssistantAttempt(_)));
    }

    /// v4's `assertCurrentLlmShape`: a reason it doesn't define, a marker
    /// other than `true`, and an adapter-default key it doesn't define.
    #[test]
    fn a_header_v4_would_refuse_is_refused() {
        let header =
            |data: Value| json!({ "type": "request/header", "seq": 0, "time": 1, "data": data });
        let config = json!({ "provider": "mock", "model": "mock" });
        for data in [
            json!({ "header": { "config": config }, "reason": "restart" }),
            json!({ "header": { "config": config }, "reason": "initial", "startsSeries": false }),
            json!({ "header": { "config": config, "adapterDefaults": { "temperature": true } },
                    "reason": "initial" }),
            json!({ "header": {}, "reason": "initial" }),
        ] {
            refusal(header(data));
        }
        assert!(
            refusal(json!({
                "type": "request/context", "seq": 0, "time": 1, "surfaceOp": "append",
                "data": { "provider": "mock", "model": "mock" },
            }))
            .contains("not surface-eligible")
        );
    }

    // "requires markers and forbids non-surface and assistant source-event
    // references"
    #[test]
    fn surface_markers_are_required_on_surface_events_and_refused_elsewhere() {
        let mut markerless = user_message();
        markerless.as_object_mut().unwrap().remove("surfaceOp");
        assert!(refusal(markerless).contains("requires a surfaceOp marker"));

        let turn = |metadata: Value| {
            let mut event =
                json!({ "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 } });
            event
                .as_object_mut()
                .unwrap()
                .extend(metadata.as_object().unwrap().clone());
            event
        };
        assert!(refusal(turn(json!({ "surfaceOp": "append" }))).contains("not surface-eligible"));
        assert!(refusal(turn(json!({ "sourceEventSeqs": [0] }))).contains("not surface-eligible"));

        let assistant = json!({
            "type": "assistant/message", "seq": 1, "time": 1, "surfaceOp": "append",
            "sourceEventSeqs": [0],
            "data": { "turn": 1, "step": 1, "stream": [], "message": {
                "role": "assistant", "content": [], "id": "m-1",
                "source": { "kind": "model", "provider": "mock", "model": "mock" },
            } },
        });
        assert!(refusal(assistant).contains("cannot carry sourceEventSeqs"));
    }

    // "rejects noncanonical replace keys"
    #[test]
    fn a_replace_operation_must_have_exactly_its_three_keys() {
        let invalid = [
            json!({ "op": "replace", "start": 0, "end": 0 }),
            json!({ "op": "replace", "start": 0, "endSeq": 0 }),
            json!({ "op": "replace", "startSeq": 0, "end": 0 }),
            json!({ "op": "replace", "startSeq": 0, "endSeq": 0, "start": 0 }),
            json!({ "op": "replace", "startSeq": 0, "endSeq": 0, "end": 0 }),
            json!({ "op": "replace", "startSeq": 0, "endSeq": 0, "extra": true }),
            json!({ "op": "replace", "startSeq": -1, "endSeq": 0 }),
            json!({ "op": "splice", "startSeq": 0, "endSeq": 0 }),
            json!("prepend"),
        ];
        for op in invalid {
            let mut event = user_message();
            event["surfaceOp"] = op.clone();
            let error = refusal(event);
            assert!(error.contains("invalid replace surfaceOp"), "{op}: {error}");
        }
    }

    #[test]
    fn source_event_seqs_must_be_a_nonempty_list_of_seqs() {
        for sources in [json!([]), json!([-1]), json!([1.5]), json!({ "seq": 0 })] {
            let mut event = user_message();
            event["sourceEventSeqs"] = sources.clone();
            assert!(read(&event).is_err(), "{sources}");
        }
    }

    // "retains opaque ignorable %s metadata without deriving messages"
    #[test]
    fn an_unknown_ignorable_event_keeps_its_opaque_metadata_and_derives_nothing() {
        for event_type in [
            "extension/event",
            "tool/code-dispatch",
            "tool/code-dispatch-start",
        ] {
            let event = json!({
                "type": event_type, "seq": 0, "time": 1, "data": { "nested": { "retained": true } },
                "ignorable": true, "surfaceOp": { "opaque": ["not", "placement"] },
                "sourceEventSeqs": { "opaque": [null, true] },
            });
            let entry = round_trip(event);
            assert!(entry.ignorable);
            let log = SessionLog::restore([entry]).unwrap();
            assert_eq!(log.surface_nodes(), []);
            assert_eq!(log.history(), crate::History::default());
        }
    }

    // "restores system prompts only with system-prompt producer attribution"
    #[test]
    fn a_system_message_must_come_from_the_system_prompt() {
        round_trip(system_message(json!({ "kind": "system-prompt" })));
        for source in [
            json!({ "kind": "runtime-context" }),
            json!({ "kind": "plugin", "plugin": "@deepseek-ai/dsh-system-prompt" }),
            json!({ "kind": "user" }),
        ] {
            assert!(read(&system_message(source.clone())).is_err(), "{source}");
        }
    }

    // surface.spec.ts: "rejects a seeded system/message with a non-system
    // role".
    #[test]
    fn a_message_whose_role_does_not_match_its_event_is_refused() {
        let mut wrong_role = system_message(json!({ "kind": "system-prompt" }));
        wrong_role["data"]["message"]["role"] = json!("user");
        assert!(refusal(wrong_role).contains("unknown variant `user`"));
    }

    // "reports malformed restored event envelopes without TypeError" and
    // session.spec.ts "rejects seed records with invalid fixed-envelope
    // fields"
    #[test]
    fn a_malformed_envelope_is_refused() {
        for value in [json!(null), json!([]), json!(1), json!("invalid")] {
            assert!(read(&value).is_err(), "{value}");
        }
        let fields = [
            ("extra", json!(true)),
            ("ignorable", json!(false)),
            ("seq", json!(-1)),
            ("seq", json!(1.5)),
            ("time", json!("now")),
        ];
        for (key, value) in fields {
            let mut event =
                json!({ "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 } });
            event[key] = value.clone();
            assert!(read(&event).is_err(), "{key}: {value}");
        }
        let missing_data = json!({ "type": "turn/start", "seq": 0, "time": 1 });
        assert!(read(&missing_data).is_err());
    }

    // session.spec.ts: "accepts and round-trips a max-tokens turn/end
    // reason" and "round-trips an aborted turn with its cancellation cause"
    #[test]
    fn turn_end_reasons_read_typed_and_write_back_unchanged() {
        let reason = |reason: Value| {
            let event = json!({
                "type": "turn/end", "seq": 0, "time": 1, "data": { "turn": 1, "reason": reason },
            });
            match round_trip(event).event {
                SessionEvent::TurnEnd(end) => end.reason,
                other => panic!("expected turn/end, got {other:?}"),
            }
        };
        assert_eq!(
            reason(json!({ "kind": "max-tokens" })),
            TurnEndReason::MaxTokens
        );
        assert_eq!(
            reason(json!({ "kind": "interrupted" })),
            TurnEndReason::Interrupted
        );
        assert_eq!(
            reason(json!({ "kind": "aborted", "reason": { "kind": "user" } })),
            TurnEndReason::Aborted(CancelCause::User)
        );
        assert_eq!(
            reason(json!({ "kind": "aborted", "reason": { "kind": "hook", "reason": "limit" } })),
            TurnEndReason::Aborted(CancelCause::Hook {
                reason: "limit".into()
            })
        );
        assert_eq!(
            reason(
                json!({ "kind": "error", "error": { "message": "boom", "code": "UNKNOWN", "status": 529 } })
            ),
            TurnEndReason::Error(Failure {
                message: "boom".into(),
                code: "UNKNOWN".into(),
                status: Some(529),
                provider_retry_after_ms: None,
                request_id: None,
                offload_images: None,
            })
        );
        // A kind a plugin added, and a known kind with a field Karaka
        // doesn't model, are kept as they were.
        let added = json!({ "kind": "budget-exhausted", "limit": 3 });
        assert_eq!(reason(added.clone()), TurnEndReason::Other(added));
        let extended = json!({ "kind": "completed", "note": "early" });
        assert_eq!(reason(extended.clone()), TurnEndReason::Other(extended));
    }

    #[test]
    fn a_tool_result_keeps_an_absent_is_error_and_a_null_meta() {
        let event = json!({
            "type": "tool/result", "seq": 1, "time": 1, "surfaceOp": "append", "sourceEventSeqs": [0],
            "data": { "turn": 1, "step": 1, "meta": null, "message": {
                "role": "tool", "source": { "kind": "tool", "callId": "c1" }, "toolCallId": "c1",
                "content": [{ "type": "text", "text": "ok" }], "id": "m-1",
            } },
        });
        let entry = round_trip(event);
        let SessionEvent::ToolResult { data, .. } = entry.event else {
            panic!("expected a tool result");
        };
        assert_eq!(data.message.is_error, None);
        assert_eq!(data.meta, Some(Value::Null));
    }

    // "successful tool has failure metadata"
    #[test]
    fn a_tool_error_on_a_result_that_did_not_fail_is_refused_when_it_joins_a_log() {
        let result = ToolResult {
            call_id: "tool".into(),
            content: String::new(),
            is_error: false,
        };
        let mut data = ToolResultData::new("m-1", 1, 1, &result);
        data.error = Some(crate::ToolError {
            name: "ToolError".into(),
            code: "FAILED".into(),
            reason: None,
            extra: crate::Extra::new(),
        });
        let event = SessionEvent::ToolResult {
            data,
            surface: crate::SurfaceIntent::append(),
        };
        let error = SessionLog::new().append(event).expect_err("refused");
        assert!(
            error
                .to_string()
                .contains("error requires message.isError === true")
        );
    }

    #[test]
    fn karaka_writes_a_tool_result_in_v4s_shape() {
        let result = ToolResult {
            call_id: "c1".into(),
            content: "12:00".into(),
            is_error: false,
        };
        let entry = Entry {
            seq: crate::Seq(4),
            time: crate::Time(1_700_000_000_000),
            ignorable: false,
            event: SessionEvent::tool_result(
                ToolResultData::new("m-9", 1, 2, &result),
                crate::Seq(3),
            ),
        };
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            json!({
                "type": "tool/result", "seq": 4, "time": 1_700_000_000_000_i64,
                "surfaceOp": "append", "sourceEventSeqs": [3],
                "data": { "turn": 1, "step": 2, "message": {
                    "id": "m-9", "role": "tool", "source": { "kind": "tool", "callId": "c1" },
                    "toolCallId": "c1", "content": [{ "type": "text", "text": "12:00" }],
                    "isError": false,
                } },
            })
        );
    }
}
