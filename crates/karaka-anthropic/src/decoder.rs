//! Anthropic's streamed events, turned into [`Event`]s.
//!
//! The [`Decoder`] only sees the JSON of each event, one at a time. Reading
//! those out of the HTTP body is a separate job, so everything here can be
//! tested without a network.

use karaka_model::{Block, ErrorKind, Event, ModelError, StopReason, ToolCall, Usage};
use serde::Deserialize;

/// Assembles one streamed response.
///
/// Anthropic sends one content block at a time: start, deltas, stop. So the
/// decoder only ever holds the block being written, never a list of them.
#[derive(Default)]
pub(crate) struct Decoder {
    open: Option<Open>,
    usage: Usage,
    stop: Option<StopReason>,
}

/// A block that has started and not yet stopped.
enum Open {
    Text(String),
    Thinking {
        text: String,
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    /// A block Karaka doesn't use, such as a server tool's. Its deltas and
    /// its stop are ignored.
    Skipped,
}

impl Decoder {
    /// Reads one event's JSON.
    ///
    /// Each Anthropic event becomes at most one [`Event`]: a delta, a
    /// finished block, or [`Event::Done`]. An error event, or JSON that
    /// doesn't fit, ends the call with an `Err`.
    pub(crate) fn feed(&mut self, data: &str) -> Result<Option<Event>, ModelError> {
        let event: WireEvent = serde_json::from_str(data).map_err(|error| {
            ModelError::new(
                ErrorKind::Unavailable,
                format!("Anthropic sent an event that couldn't be read: {error}"),
            )
        })?;

        Ok(match event {
            WireEvent::MessageStart { message } => {
                self.add_usage(message.usage);
                None
            }
            WireEvent::ContentBlockStart { content_block } => {
                self.open = Some(match content_block {
                    StartBlock::Text { text } => Open::Text(text),
                    StartBlock::Thinking {
                        thinking,
                        signature,
                    } => Open::Thinking {
                        text: thinking,
                        signature,
                    },
                    StartBlock::ToolUse { id, name } => Open::ToolUse {
                        id,
                        name,
                        json: String::new(),
                    },
                    StartBlock::Other => Open::Skipped,
                });
                None
            }
            WireEvent::ContentBlockDelta { delta } => self.delta(delta)?,
            WireEvent::ContentBlockStop => {
                let open = self
                    .open
                    .take()
                    .ok_or_else(|| protocol("a block stopped"))?;
                finish(open).map(Event::Block)
            }
            WireEvent::MessageDelta { delta, usage } => {
                self.add_usage(usage);
                if let Some(reason) = delta.stop_reason {
                    self.stop = Some(stop_reason(reason));
                }
                None
            }
            WireEvent::MessageStop => {
                let stop = self.stop.take().ok_or_else(|| {
                    ModelError::new(
                        ErrorKind::Unavailable,
                        "Anthropic finished without saying why it stopped",
                    )
                })?;
                Some(Event::Done {
                    stop,
                    usage: self.usage,
                })
            }
            WireEvent::Error { error } => {
                return Err(ModelError::new(error_kind(&error.kind), error.message));
            }
            WireEvent::Ping | WireEvent::Other => None,
        })
    }

    fn delta(&mut self, delta: Delta) -> Result<Option<Event>, ModelError> {
        let open = self
            .open
            .as_mut()
            .ok_or_else(|| protocol("a delta arrived"))?;
        Ok(match (open, delta) {
            (Open::Text(text), Delta::Text { text: more }) => {
                text.push_str(&more);
                Some(Event::TextDelta(more))
            }
            (Open::Thinking { text, .. }, Delta::Thinking { thinking: more }) => {
                text.push_str(&more);
                Some(Event::ReasoningDelta(more))
            }
            (Open::Thinking { signature, .. }, Delta::Signature { signature: more }) => {
                signature.push_str(&more);
                None
            }
            // Tool arguments arrive as pieces of JSON that only parse once
            // they're all there, so they aren't passed on one by one.
            (Open::ToolUse { json, .. }, Delta::InputJson { partial_json }) => {
                json.push_str(&partial_json);
                None
            }
            (Open::Skipped, _) | (_, Delta::Other) => None,
            _ => return Err(protocol("a delta of the wrong kind arrived")),
        })
    }

    fn add_usage(&mut self, wire: WireUsage) {
        // Anthropic's counts are running totals, so a later value replaces an
        // earlier one rather than adding to it.
        let usage = &mut self.usage;
        let fields = [
            (&mut usage.input_tokens, wire.input_tokens),
            (&mut usage.output_tokens, wire.output_tokens),
            (&mut usage.cache_read_tokens, wire.cache_read_input_tokens),
            (
                &mut usage.cache_write_tokens,
                wire.cache_creation_input_tokens,
            ),
        ];
        for (field, value) in fields {
            if let Some(value) = value {
                *field = value;
            }
        }
    }
}

/// A finished block, or `None` for one Karaka skips.
fn finish(open: Open) -> Option<Block> {
    Some(match open {
        Open::Text(text) => Block::Text(text),
        // Kept even when the text is empty, as it is unless thinking is set
        // to be shown: the signature still has to go back next turn.
        Open::Thinking { text, signature } => Block::Reasoning {
            text,
            replay: Some(signature),
        },
        Open::ToolUse { id, name, json } => Block::ToolCall(ToolCall {
            id,
            name,
            // A tool without arguments may send no JSON at all.
            arguments: if json.is_empty() { "{}".into() } else { json },
        }),
        Open::Skipped => return None,
    })
}

fn stop_reason(reason: String) -> StopReason {
    match reason.as_str() {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolCalls,
        "max_tokens" => StopReason::MaxTokens,
        // `refusal`, `pause_turn` and whatever comes next keep their own
        // name. The loop runs tools only on `ToolCalls`, which matters: a
        // refusal can cut a tool call off halfway.
        _ => StopReason::Other(reason),
    }
}

/// The kind of failure behind one of Anthropic's error types. The same types
/// come back in the body of a failed HTTP response.
pub(crate) fn error_kind(kind: &str) -> ErrorKind {
    match kind {
        "authentication_error" | "permission_error" | "billing_error" => ErrorKind::Auth,
        "invalid_request_error" | "not_found_error" | "request_too_large" => {
            ErrorKind::InvalidRequest
        }
        "rate_limit_error" => ErrorKind::RateLimited,
        // `api_error`, `overloaded_error`, and any type that's new to us.
        _ => ErrorKind::Unavailable,
    }
}

fn protocol(what: &str) -> ModelError {
    ModelError::new(
        ErrorKind::Unavailable,
        format!("Anthropic's stream broke its own rules: {what} with no block open"),
    )
}

// What Anthropic sends. Only the fields Karaka reads are declared; serde
// skips the rest. `#[serde(other)]` catches event, block and delta types
// added after this was written, so a new one is ignored rather than fatal.

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireEvent {
    MessageStart {
        message: StartMessage,
    },
    ContentBlockStart {
        content_block: StartBlock,
    },
    ContentBlockDelta {
        delta: Delta,
    },
    ContentBlockStop,
    MessageDelta {
        delta: MessageDelta,
        #[serde(default)]
        usage: WireUsage,
    },
    MessageStop,
    Ping,
    Error {
        error: WireError,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct StartMessage {
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StartBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct MessageDelta {
    stop_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct WireUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

/// Also the `error` field of a failed HTTP response's body.
#[derive(Deserialize)]
pub(crate) struct WireError {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) message: String,
}

#[cfg(test)]
mod tests {
    use karaka_model::{Block, ErrorKind, Event, ModelError, StopReason, ToolCall, Usage};

    use super::Decoder;

    /// Feeds every event and keeps what comes out.
    fn decode(events: &[&str]) -> Result<Vec<Event>, ModelError> {
        let mut decoder = Decoder::default();
        let mut out = Vec::new();
        for event in events {
            out.extend(decoder.feed(event)?);
        }
        Ok(out)
    }

    const START: &str = r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":20,"output_tokens":1,"cache_read_input_tokens":100,"cache_creation_input_tokens":0}}}"#;

    #[test]
    fn a_text_answer() {
        let events = decode(&[
            START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"ping"}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}"#,
            r#"{"type":"message_stop"}"#,
        ])
        .expect("the stream decodes");

        assert_eq!(
            events,
            [
                Event::TextDelta("Hel".into()),
                Event::TextDelta("lo".into()),
                Event::Block(Block::Text("Hello".into())),
                Event::Done {
                    stop: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 5,
                        cache_read_tokens: 100,
                        cache_write_tokens: 0,
                    },
                },
            ]
        );
    }

    #[test]
    fn thinking_then_a_tool_call() {
        let events = decode(&[
            START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"check the clock"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"clock","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"zone\": "}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"UTC\"}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":30}}"#,
            r#"{"type":"message_stop"}"#,
        ])
        .expect("the stream decodes");

        assert_eq!(events[0], Event::ReasoningDelta("check the clock".into()));
        assert_eq!(
            events[1],
            Event::Block(Block::Reasoning {
                text: "check the clock".into(),
                replay: Some("sig".into()),
            })
        );
        assert_eq!(
            events[2],
            Event::Block(Block::ToolCall(ToolCall {
                id: "toolu_1".into(),
                name: "clock".into(),
                arguments: r#"{"zone": "UTC"}"#.into(),
            }))
        );
        assert!(matches!(
            events[3],
            Event::Done {
                stop: StopReason::ToolCalls,
                ..
            }
        ));
    }

    #[test]
    fn new_kinds_of_events_and_blocks_are_skipped() {
        let events = decode(&[
            START,
            r#"{"type":"some_future_event","detail":{"a":1}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv_1","name":"web_search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal"},"usage":{"output_tokens":2}}"#,
            r#"{"type":"message_stop"}"#,
        ])
        .expect("the stream decodes");

        let [Event::Done { stop, .. }] = events.as_slice() else {
            panic!("expected only Done, got {events:?}");
        };
        assert_eq!(*stop, StopReason::Other("refusal".into()));
    }

    #[test]
    fn an_error_event_ends_the_call() {
        let error = decode(&[
            START,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ])
        .expect_err("the call fails");

        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert_eq!(error.message, "Overloaded");
    }

    #[test]
    fn a_delta_with_no_open_block_is_an_error() {
        let error = decode(&[
            START,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        ])
        .expect_err("the call fails");

        assert!(error.message.contains("no block open"), "{error}");
    }
}
