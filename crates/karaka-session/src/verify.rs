//! The saved-stream check: each logged answer agrees with the stream it
//! embeds. (dsh: `assertCurrentAssistantStreams` in
//! `session-persistence-jsonl/src/generation.ts`.)
//!
//! It replays every stream through the [`BlockAssembler`], so it costs as
//! much as the streams are long. dsh runs it only when it verifies a
//! migrated generation, never on an ordinary open, and neither does
//! Karaka: it is a check for tests and tools to run on a log.

use karaka_model::BlockAssembler;
use serde_json::Value;

use crate::event::{Entry, SessionEvent};
use crate::message::AssistantMessage;
use crate::stream::{chunk_event, expand_stream};

/// Checks that every `assistant/message` and `assistant/attempt` in
/// `entries` embeds a stream that expands, and that each message's
/// content, usage and replay state are what its stream assembles to. An
/// interrupted message is checked against the blocks a cancel keeps. A
/// message with an empty stream is not checked. (dsh:
/// `assertCurrentAssistantStreams`, in its words.)
pub fn check_assistant_streams(entries: &[Entry]) -> Result<(), String> {
    for (index, entry) in entries.iter().enumerate() {
        let (kind, stream, message) = match &entry.event {
            SessionEvent::AssistantMessage { data, .. } => {
                ("assistant/message", &data.stream, Some(data))
            }
            SessionEvent::AssistantAttempt(data) => ("assistant/attempt", &data.stream, None),
            _ => continue,
        };
        let timed = expand_stream(stream)
            .map_err(|_| format!("seed {kind} at index {index} has an invalid embedded stream"))?;
        let mut assembler = BlockAssembler::new();
        // The usage and the replay state are compared as the stream wrote
        // them, as dsh's assembler keeps them: the last `usage` chunk's
        // object, and the finish's `replayState`.
        let mut usage = None;
        let mut replay_state = None;
        for chunk in &timed {
            match chunk.chunk.get("type").and_then(Value::as_str) {
                Some("usage") => usage = chunk.chunk.get("usage").cloned(),
                Some("finish") => replay_state = chunk.chunk.get("replayState").cloned(),
                _ => {}
            }
            if let Some(event) = chunk_event(&chunk.chunk) {
                assembler.push(&event);
            }
        }
        let Some(data) = message else {
            continue;
        };
        if timed.is_empty() {
            continue;
        }
        let blocks = if data.interrupted {
            assembler.interrupted_blocks()
        } else {
            assembler.blocks()
        };
        // The assembled blocks in v4's content form, as the writer turns
        // an answer into a message.
        let source = &data.message.source;
        let assembled = AssistantMessage::from_model("", &source.provider, &source.model, &blocks);
        if json(&data.message.content) != json(&assembled.content) {
            return Err(format!(
                "seed assistant/message at index {index} content disagrees with its embedded stream"
            ));
        }
        if data.usage.as_ref().map(json) != usage {
            return Err(format!(
                "seed assistant/message at index {index} usage disagrees with its embedded stream"
            ));
        }
        if source.replay_state != replay_state {
            return Err(format!(
                "seed assistant/message at index {index} replay state disagrees with its embedded \
                 stream"
            ));
        }
    }
    Ok(())
}

fn json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::check_assistant_streams;
    use crate::Entry;

    fn answer(stream: serde_json::Value, extra: serde_json::Value) -> Vec<Entry> {
        let mut data = json!({
            "turn": 1, "step": 1,
            "message": { "id": "a", "role": "assistant", "content": [{ "type": "text", "text": "hello" }],
                         "source": { "kind": "model", "provider": "mock", "model": "m" } },
            "stream": stream
        });
        data.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(json!([
            { "type": "assistant/message", "seq": 0, "time": 1, "surfaceOp": "append", "data": data }
        ]))
        .unwrap()
    }

    fn hello(finish: serde_json::Value) -> serde_json::Value {
        json!([
            { "type": "chunk", "time": 1, "chunk": { "type": "block-start", "index": 0, "blockType": "text" } },
            { "type": "text-chunks", "time0": 1, "index": 0, "dt": [], "texts": ["hello"] },
            { "type": "chunk", "time": 2, "chunk": { "type": "block-end", "index": 0, "block": { "type": "text", "text": "hello" } } },
            { "type": "chunk", "time": 2, "chunk": finish }
        ])
    }

    fn stop() -> serde_json::Value {
        json!({ "type": "finish", "reason": { "kind": "stop" } })
    }

    #[test]
    fn an_answer_that_its_stream_assembles_to_passes() {
        assert_eq!(
            check_assistant_streams(&answer(hello(stop()), json!({}))),
            Ok(())
        );
        // An empty stream isn't checked.
        assert_eq!(
            check_assistant_streams(&answer(json!([]), json!({}))),
            Ok(())
        );
    }

    #[test]
    fn a_disagreement_is_refused_in_dshs_words() {
        let other = json!([
            { "type": "chunk", "time": 1, "chunk": { "type": "block-start", "index": 0, "blockType": "text" } },
            { "type": "text-chunks", "time0": 1, "index": 0, "dt": [], "texts": ["bye"] },
            { "type": "chunk", "time": 2, "chunk": stop() }
        ]);
        assert_eq!(
            check_assistant_streams(&answer(other, json!({}))),
            Err(
                "seed assistant/message at index 0 content disagrees with its embedded stream"
                    .into()
            )
        );
        let usage = json!({ "usage": { "inputTokens": 1, "outputTokens": 1 } });
        assert_eq!(
            check_assistant_streams(&answer(hello(stop()), usage)),
            Err(
                "seed assistant/message at index 0 usage disagrees with its embedded stream".into()
            )
        );
        let signed =
            json!({ "type": "finish", "reason": { "kind": "stop" }, "replayState": { "v": 2 } });
        assert_eq!(
            check_assistant_streams(&answer(hello(signed), json!({}))),
            Err(
                "seed assistant/message at index 0 replay state disagrees with its embedded stream"
                    .into()
            )
        );
        let broken =
            json!([{ "type": "text-chunks", "time0": 1, "index": 0, "dt": [5], "texts": ["a"] }]);
        assert_eq!(
            check_assistant_streams(&answer(broken, json!({}))),
            Err("seed assistant/message at index 0 has an invalid embedded stream".into())
        );
    }
}
