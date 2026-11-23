//! The compact `stream` records, ported from dsh's
//! `llm/tests/assistant-stream.spec.ts`. The chunks and records are dsh's
//! own JSON, so each case reads as it does there.

use karaka_model::{
    Block, BlockAssembler, BlockType, Event, FinishReason, ToolCall, Usage, stream_of,
};
use karaka_session::{
    Failure, StreamAccumulator, Time, TimedChunk, chunk_event, chunk_json, expand_stream,
    failure_json, finish_json,
};
use serde_json::{Value, json};

const MAX_SAFE: i64 = 9_007_199_254_740_991;

fn timed(time: i64, chunk: Value) -> TimedChunk {
    TimedChunk {
        time: Time(time),
        chunk,
    }
}

fn accumulate(chunks: &[TimedChunk]) -> Vec<Value> {
    let mut accumulator = StreamAccumulator::new();
    for chunk in chunks {
        accumulator
            .push(chunk.time, chunk.chunk.clone())
            .expect("the chunk is valid");
    }
    accumulator.snapshot()
}

/// Compares as JSON text, so key order counts too.
fn same_text(left: &[Value], right: &[Value]) {
    assert_eq!(
        serde_json::to_string(left).unwrap(),
        serde_json::to_string(right).unwrap()
    );
}

// "keeps delta boundaries and timestamps while compacting one attempt"
#[test]
fn deltas_pack_into_runs_and_expand_back_exactly() {
    let chunks = [
        timed(
            1_000,
            json!({ "type": "text-delta", "index": 0, "text": "hel" }),
        ),
        timed(
            1_006,
            json!({ "type": "text-delta", "index": 0, "text": "lo" }),
        ),
        timed(
            1_008,
            json!({ "type": "tool-call-delta", "index": 1, "id": "call-1", "name": "run_code", "argumentsDelta": "{" }),
        ),
        timed(
            1_011,
            json!({ "type": "tool-call-delta", "index": 1, "id": "call-1", "name": "run_code", "argumentsDelta": "}" }),
        ),
        timed(
            1_020,
            json!({ "type": "finish", "reason": { "kind": "stop" } }),
        ),
    ];
    let records = accumulate(&chunks);
    same_text(
        &records,
        &[
            json!({ "type": "text-chunks", "time0": 1_000, "index": 0, "dt": [6], "texts": ["hel", "lo"] }),
            json!({ "type": "tool-call-chunks", "time0": 1_008, "index": 1, "dt": [3], "id": "call-1", "name": "run_code", "args": ["{", "}"] }),
            json!({ "type": "chunk", "time": 1_020, "chunk": { "type": "finish", "reason": { "kind": "stop" } } }),
        ],
    );
    assert_eq!(expand_stream(&records).unwrap(), chunks);
}

// "keeps incompatible delta runs separate and expands reasoning and
// nameless tool calls"
#[test]
fn incompatible_deltas_start_new_runs() {
    let chunks = [
        timed(
            1,
            json!({ "type": "reasoning-delta", "index": 0, "text": "r1" }),
        ),
        timed(
            2,
            json!({ "type": "reasoning-delta", "index": 0, "text": "r2" }),
        ),
        timed(3, json!({ "type": "text-delta", "index": 0, "text": "a" })),
        timed(4, json!({ "type": "text-delta", "index": 1, "text": "b" })),
        timed(
            5,
            json!({ "type": "tool-call-delta", "index": 0, "id": "one", "argumentsDelta": "{" }),
        ),
        timed(
            6,
            json!({ "type": "tool-call-delta", "index": 0, "id": "one", "argumentsDelta": "}" }),
        ),
        timed(
            7,
            json!({ "type": "tool-call-delta", "index": 0, "id": "one", "name": "read", "argumentsDelta": "" }),
        ),
        timed(
            8,
            json!({ "type": "tool-call-delta", "index": 1, "id": "two", "name": "read", "argumentsDelta": "x" }),
        ),
        timed(
            MAX_SAFE,
            json!({ "type": "text-delta", "index": 1, "text": "far" }),
        ),
        timed(
            -MAX_SAFE,
            json!({ "type": "text-delta", "index": 1, "text": "apart" }),
        ),
    ];
    let records = accumulate(&chunks);
    assert_eq!(expand_stream(&records).unwrap(), chunks);
    let types: Vec<&str> = records
        .iter()
        .map(|record| record["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "reasoning-chunks",
            "text-chunks",
            "text-chunks",
            "tool-call-chunks",
            "tool-call-chunks",
            "tool-call-chunks",
            "text-chunks",
            "text-chunks",
        ]
    );
}

// "rejects unsafe values while preserving JSON-safe empty adapter
// identities"
#[test]
fn unsafe_values_are_refused_and_empty_identities_kept_raw() {
    let mut accumulator = StreamAccumulator::new();
    let refusal = |accumulator: &mut StreamAccumulator, time: i64, chunk: Value| {
        accumulator
            .push(Time(time), chunk)
            .expect_err("the chunk is refused")
            .to_string()
    };
    assert!(
        refusal(
            &mut accumulator,
            MAX_SAFE + 1,
            json!({ "type": "finish", "reason": { "kind": "stop" } })
        )
        .contains("safe integer")
    );
    assert!(
        refusal(
            &mut accumulator,
            1,
            json!({ "type": "text-delta", "index": -1, "text": "bad" })
        )
        .contains("index")
    );
    assert!(
        refusal(
            &mut accumulator,
            1,
            json!({ "type": "text-delta", "index": 0, "text": 1 })
        )
        .contains("text must be a string")
    );
    accumulator
        .push(
            Time(1),
            json!({ "type": "tool-call-delta", "index": 0, "id": "", "argumentsDelta": "{}" }),
        )
        .unwrap();
    accumulator
        .push(
            Time(1),
            json!({ "type": "tool-call-delta", "index": 0, "id": "call", "name": "", "argumentsDelta": "{}" }),
        )
        .unwrap();
    assert!(
        refusal(
            &mut accumulator,
            1,
            json!({ "type": "tool-call-delta", "index": 0, "id": 1, "argumentsDelta": "{}" })
        )
        .contains("id must be a string")
    );
    assert!(
        refusal(
            &mut accumulator,
            1,
            json!({ "type": "tool-call-delta", "index": 0, "id": "call", "name": 1, "argumentsDelta": "{}" })
        )
        .contains("name must be a string")
    );
    assert!(
        refusal(
            &mut accumulator,
            1,
            json!({ "type": "tool-call-delta", "index": 0, "id": "call", "argumentsDelta": 1 })
        )
        .contains("argumentsDelta must be a string")
    );
    let records = accumulator.snapshot();
    same_text(
        &records,
        &[
            json!({ "type": "chunk", "time": 1, "chunk": { "type": "tool-call-delta", "index": 0, "id": "", "argumentsDelta": "{}" } }),
            json!({ "type": "chunk", "time": 1, "chunk": { "type": "tool-call-delta", "index": 0, "id": "call", "name": "", "argumentsDelta": "{}" } }),
        ],
    );
}

// "rejects malformed compact record %#"
#[test]
fn malformed_records_are_refused() {
    let cases = [
        (json!(null), "must be an object"),
        (json!([]), "must be an object"),
        (json!({ "type": "future" }), "Unsupported"),
        (
            json!({ "type": "text-chunks", "time0": 1, "index": 0, "dt": [], "texts": [] }),
            "non-empty",
        ),
        (
            json!({ "type": "reasoning-chunks", "time0": 1, "index": 0, "dt": [], "texts": [1] }),
            "string array",
        ),
        (
            json!({ "type": "text-chunks", "time0": 0.5, "index": 0, "dt": [], "texts": ["a"] }),
            "safe integer",
        ),
        (
            json!({ "type": "text-chunks", "time0": 1, "index": -0.0, "dt": [], "texts": ["a"] }),
            "index",
        ),
        (
            json!({ "type": "text-chunks", "time0": 1, "index": -1, "dt": [], "texts": ["a"] }),
            "index",
        ),
        (
            json!({ "type": "text-chunks", "time0": 1, "index": 0, "dt": [0.5], "texts": ["a", "b"] }),
            "dt",
        ),
        (
            json!({ "type": "text-chunks", "time0": 1, "index": 0, "dt": [1], "texts": ["a"] }),
            "dt length",
        ),
        (
            json!({ "type": "text-chunks", "time0": MAX_SAFE, "index": 0, "dt": [1], "texts": ["a", "b"] }),
            "member times",
        ),
        (
            json!({ "type": "tool-call-chunks", "time0": 1, "index": 0, "id": "", "dt": [], "args": ["a"] }),
            "id",
        ),
        (
            json!({ "type": "tool-call-chunks", "time0": 1, "index": 0, "id": "id", "name": "", "dt": [], "args": ["a"] }),
            "name",
        ),
        (
            json!({ "type": "tool-call-chunks", "time0": 1, "index": 0, "id": "id", "dt": [], "args": [] }),
            "non-empty",
        ),
        (
            json!({ "type": "tool-call-chunks", "time0": 1, "index": 0, "id": "id", "dt": [], "args": [1] }),
            "string array",
        ),
        (
            json!({ "type": "chunk", "time": 0.5, "chunk": { "type": "finish", "reason": { "kind": "stop" } } }),
            "safe integer",
        ),
        (
            json!({ "type": "chunk", "time": 1, "chunk": null }),
            "lossless JSON object",
        ),
        (
            json!({ "type": "chunk", "time": 1, "chunk": [] }),
            "lossless JSON object",
        ),
        (
            json!({ "type": "text-chunks", "time0": 1, "index": 0, "dt": [], "texts": ["a"], "extra": true }),
            "exactly",
        ),
        (
            json!({ "type": "chunk", "time": 1, "chunk": { "type": "finish", "reason": { "kind": "stop" } }, "extra": true }),
            "exactly",
        ),
    ];
    for (record, message) in cases {
        let refused = expand_stream(std::slice::from_ref(&record))
            .expect_err("the record is refused")
            .to_string();
        assert!(refused.contains(message), "{record}: {refused}");
    }
}

/// A record a dsh adapter wrote keeps everything Karaka doesn't model: a
/// finish with its `replayState`, an error finish, a block Karaka has no
/// type for.
#[test]
fn records_dsh_wrote_expand_without_losing_anything() {
    let finish = json!({
        "type": "finish",
        "reason": { "kind": "tool-calls" },
        "replayState": { "response": { "id": "r" }, "blocks": [{ "type": "tool-call" }] },
        "note": "a field Karaka doesn't know",
    });
    let records = [
        json!({ "type": "chunk", "time": 5, "chunk": { "type": "block-start", "index": 0, "blockType": "image" } }),
        json!({ "type": "chunk", "time": 6, "chunk": finish }),
        json!({ "type": "chunk", "time": 7, "chunk": { "type": "finish", "reason": { "kind": "error", "failure": { "message": "busy", "code": "RATE_LIMIT", "status": 429 } } } }),
    ];
    let expanded = expand_stream(&records).unwrap();
    assert_eq!(expanded[1].chunk, records[1]["chunk"]);
    // Packed again, the records come back as they were.
    same_text(&accumulate(&expanded), &records);
    // The typed view reads what Karaka models and nothing else.
    assert_eq!(chunk_event(&expanded[0].chunk), None);
    assert_eq!(
        chunk_event(&expanded[1].chunk),
        Some(Event::Finish {
            reason: FinishReason::ToolCalls
        })
    );
    assert_eq!(chunk_event(&expanded[2].chunk), None);
}

/// Karaka's chunks, written as dsh's JSON, packed, and expanded again,
/// give the exact chunks back, and read back as the same chunks.
#[test]
fn karaka_chunks_round_trip_through_the_records() {
    let call = ToolCall {
        id: "call-1".into(),
        name: "read".into(),
        arguments: r#"{"path":"a"}"#.into(),
    };
    let mut events = stream_of(
        &[Block::Text("answer".into()), Block::ToolCall(call.clone())],
        FinishReason::ToolCalls,
        Usage {
            input_tokens: 3,
            output_tokens: 2,
            cache_read_tokens: 1,
            cache_write_tokens: 0,
        },
    );
    // A second piece of the text and of the arguments, so the runs pack.
    events.insert(
        2,
        Event::TextDelta {
            index: 0,
            text: "!".into(),
        },
    );
    let chunks: Vec<TimedChunk> = events
        .iter()
        .enumerate()
        .map(|(n, event)| timed(1_000 + n as i64, chunk_json(event)))
        .collect();
    let records = accumulate(&chunks);
    assert_eq!(records[1]["type"], "text-chunks");
    assert_eq!(records[1]["texts"], json!(["answer", "!"]));
    let expanded = expand_stream(&records).unwrap();
    assert_eq!(expanded, chunks);
    let back: Vec<Event> = expanded
        .iter()
        .map(|chunk| chunk_event(&chunk.chunk).expect("a chunk Karaka models"))
        .collect();
    assert_eq!(back, events);
}

/// dsh's JSON for each chunk, field for field.
#[test]
fn chunks_are_written_as_dshs_json() {
    let lines: Vec<String> = [
        Event::BlockStart {
            index: 0,
            block_type: BlockType::Reasoning,
        },
        Event::ToolCallDelta {
            index: 1,
            id: "c".into(),
            name: None,
            arguments_delta: "{".into(),
        },
        Event::BlockEnd {
            index: 0,
            block: Block::Reasoning {
                text: "why".into(),
                replay: None,
            },
        },
        Event::Usage {
            usage: Usage {
                input_tokens: 3,
                output_tokens: 2,
                ..Usage::default()
            },
        },
        Event::Finish {
            reason: FinishReason::MaxTokens,
        },
    ]
    .iter()
    .map(|event| chunk_json(event).to_string())
    .collect();
    assert_eq!(
        lines,
        [
            r#"{"type":"block-start","index":0,"blockType":"reasoning"}"#,
            r#"{"type":"tool-call-delta","index":1,"id":"c","argumentsDelta":"{"}"#,
            r#"{"type":"block-end","index":0,"block":{"type":"reasoning","text":"why"}}"#,
            r#"{"type":"usage","usage":{"inputTokens":3,"outputTokens":2,"cacheReadTokens":0,"cacheWriteTokens":0}}"#,
            r#"{"type":"finish","reason":{"kind":"max-tokens"}}"#,
        ]
    );
    let state = json!({ "response": { "kind": "pi-ai" }, "blocks": [] });
    assert_eq!(
        finish_json(FinishReason::Stop, Some(&state)).to_string(),
        r#"{"type":"finish","reason":{"kind":"stop"},"replayState":{"response":{"kind":"pi-ai"},"blocks":[]}}"#
    );
    let failure = Failure {
        message: "busy".into(),
        code: "RATE_LIMIT".into(),
        status: None,
        provider_retry_after_ms: Some(2000.into()),
        request_id: None,
        offload_images: None,
    };
    assert_eq!(
        failure_json(&failure).to_string(),
        r#"{"type":"finish","reason":{"kind":"error","failure":{"message":"busy","code":"RATE_LIMIT","providerRetryAfterMs":2000}}}"#
    );
}

// "assembles the same blocks, usage, finish, and replay state as the
// expanded members": the records assemble to the same answer as the
// chunks did.
#[test]
fn the_records_assemble_to_the_same_answer() {
    let events = vec![
        Event::BlockStart {
            index: 0,
            block_type: BlockType::Reasoning,
        },
        Event::ReasoningDelta {
            index: 0,
            text: "th".into(),
        },
        Event::ReasoningDelta {
            index: 0,
            text: "ink".into(),
        },
        Event::BlockEnd {
            index: 0,
            block: Block::Reasoning {
                text: "think".into(),
                replay: None,
            },
        },
        Event::TextDelta {
            index: 1,
            text: "an".into(),
        },
        Event::TextDelta {
            index: 1,
            text: "swer".into(),
        },
        Event::ToolCallDelta {
            index: 2,
            id: "call-1".into(),
            name: Some("read".into()),
            arguments_delta: String::new(),
        },
        Event::ToolCallDelta {
            index: 2,
            id: "call-1".into(),
            name: None,
            arguments_delta: r#"{"path":"#.into(),
        },
        Event::ToolCallDelta {
            index: 2,
            id: "call-1".into(),
            name: None,
            arguments_delta: r#""a"}"#.into(),
        },
        Event::ToolCallDelta {
            index: 3,
            id: String::new(),
            name: None,
            arguments_delta: "{}".into(),
        },
        Event::Usage {
            usage: Usage {
                input_tokens: 3,
                output_tokens: 2,
                ..Usage::default()
            },
        },
        Event::Finish {
            reason: FinishReason::ToolCalls,
        },
    ];
    let chunks: Vec<TimedChunk> = events
        .iter()
        .enumerate()
        .map(|(n, event)| timed(1_000 + n as i64, chunk_json(event)))
        .collect();
    let records = accumulate(&chunks);
    let runs = records
        .iter()
        .filter(|record| record["type"] != "chunk")
        .count();
    assert_eq!(runs, 4);

    let mut original = BlockAssembler::new();
    for event in &events {
        original.push(event);
    }
    let mut replayed = BlockAssembler::new();
    for chunk in expand_stream(&records).unwrap() {
        replayed.push(&chunk_event(&chunk.chunk).unwrap());
    }
    assert_eq!(replayed.blocks(), original.blocks());
    assert_eq!(replayed.usage(), original.usage());
    assert_eq!(replayed.finish(), original.finish());
}
