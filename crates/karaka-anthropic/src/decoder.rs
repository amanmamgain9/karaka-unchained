//! Anthropic's streamed events, turned into [`Event`]s.
//!
//! The [`Decoder`] only sees the JSON of each event, one at a time. Reading
//! those out of the HTTP body is a separate job, so everything here can be
//! tested without a network.

use karaka_model::{
    Block, ErrorKind, Event, ModelError, Refusal, StopReason, ToolCall, Usage, is_context_overflow,
    is_out_of_quota,
};
use serde::Deserialize;

/// Assembles one streamed response.
///
/// Anthropic sends one content block at a time: start, deltas, stop. So the
/// decoder holds the block being written, and only the finished blocks that
/// must wait for the stop reason.
#[derive(Default)]
pub(crate) struct Decoder {
    open: Option<Open>,
    /// Finished blocks waiting for the stop reason.
    ///
    /// Anthropic stops each block before it says why the model stopped, but
    /// a tool call cut off by `max_tokens` must never reach anyone (see
    /// [`Event`]). So a finished tool call waits here until `message_stop`,
    /// and every block after it waits too, to keep the blocks in order.
    /// Blocks before the first tool call go out as soon as they finish.
    ///
    /// A refusal can cut a tool call off as well. It fails the call at the
    /// stop reason, so held calls never go out then either.
    held: Vec<Block>,
    /// Whether any block has finished, whether it went out or is held. A
    /// normal stop with none is an empty response.
    any_block: bool,
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
    /// Returns a list because `message_stop` releases every held block and
    /// then [`Event::Done`] at once. Every other event gives one event or
    /// none, and an empty `Vec` costs no allocation.
    ///
    /// An error event, a stop reason that isn't a finished answer, or JSON
    /// that doesn't fit ends the call with an `Err`.
    pub(crate) fn feed(&mut self, data: &str) -> Result<Vec<Event>, ModelError> {
        // dsh calls a stream it can't read malformed
        // (`llm-deepseek/src/sse.ts:18-19`), and so doesn't retry it.
        let event: WireEvent = serde_json::from_str(data).map_err(|error| {
            ModelError::new(
                ErrorKind::Malformed,
                format!("Anthropic sent an event that couldn't be read: {error}"),
            )
        })?;

        Ok(match event {
            WireEvent::MessageStart { message } => {
                self.add_usage(message.usage);
                Vec::new()
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
                Vec::new()
            }
            WireEvent::ContentBlockDelta { delta } => self.delta(delta)?.into_iter().collect(),
            WireEvent::ContentBlockStop => {
                let open = self
                    .open
                    .take()
                    .ok_or_else(|| protocol("a block stopped"))?;
                match finish(open) {
                    Some(block) => self.finished(block),
                    None => Vec::new(),
                }
            }
            WireEvent::MessageDelta { delta, usage } => {
                self.add_usage(usage);
                if let Some(reason) = delta.stop_reason {
                    self.stop = Some(stop_reason(&reason, delta.stop_details)?);
                }
                Vec::new()
            }
            WireEvent::MessageStop => self.done()?,
            WireEvent::Error { error } => return Err(error.into_model_error(None)),
            WireEvent::Ping | WireEvent::Other => Vec::new(),
        })
    }

    /// Sends a finished block on, or holds it for the stop reason (see
    /// [`Decoder::held`]).
    fn finished(&mut self, block: Block) -> Vec<Event> {
        self.any_block = true;
        if matches!(block, Block::ToolCall(_)) || !self.held.is_empty() {
            self.held.push(block);
            Vec::new()
        } else {
            vec![Event::Block(block)]
        }
    }

    /// The held blocks, then [`Event::Done`]. On `max_tokens` the tool calls
    /// among them are dropped, as dsh drops them (`llm/src/assembler.ts:137-141`).
    fn done(&mut self) -> Result<Vec<Event>, ModelError> {
        let stop = self.stop.take().ok_or_else(|| {
            // llm-deepseek's `translate.ts:148`.
            ModelError::new(
                ErrorKind::Malformed,
                "Anthropic finished without saying why it stopped",
            )
        })?;
        // A finished answer with nothing in it would end the turn with
        // nothing to show or act on. dsh retries it instead
        // (`llm-pi-ai/src/stream.ts:96-107`, `llm-deepseek/src/translate.ts:149`).
        // Any block counts, so an answer that is only thinking is kept.
        if stop == StopReason::EndTurn && !self.any_block {
            return Err(ModelError::new(
                ErrorKind::EmptyResponse,
                "Anthropic finished an answer with no content",
            ));
        }
        let mut events: Vec<Event> = std::mem::take(&mut self.held)
            .into_iter()
            .filter(|block| stop != StopReason::MaxTokens || !matches!(block, Block::ToolCall(_)))
            .map(Event::Block)
            .collect();
        events.push(Event::Done {
            stop,
            usage: self.usage,
        });
        Ok(events)
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

/// What a stop reason means, mapped as pi-ai maps it
/// (`api/anthropic-messages.js:1285-1307`). Only some reasons are an answer.
/// The others fail the call, even though they come in a successful
/// response.
fn stop_reason(reason: &str, details: Option<StopDetails>) -> Result<StopReason, ModelError> {
    match reason {
        // Karaka sends no stop sequences, so `stop_sequence` shouldn't come.
        // If it does, the answer ended where the request asked.
        "end_turn" | "stop_sequence" => Ok(StopReason::EndTurn),
        "tool_use" => Ok(StopReason::ToolCalls),
        "max_tokens" => Ok(StopReason::MaxTokens),
        // A server-tools turn that Anthropic paused and will resume. Not a
        // failure, so it isn't an error. pi-ai calls it a normal stop; Karaka
        // keeps it apart, because the turn isn't finished.
        "pause_turn" => Ok(StopReason::Paused),
        "refusal" => Err(refusal(details)),
        // A safety filter flagged the answer. Newer than the SDK's types.
        "sensitive" => Err(ModelError::new(
            ErrorKind::Refused,
            "Anthropic's safety filter stopped the answer",
        )),
        // pi-ai has no case for it, but the error it then raises names the
        // reason, which its own overflow patterns match (`utils/overflow.js:55`).
        "model_context_window_exceeded" => Err(ModelError::new(
            ErrorKind::ContextWindowExceeded,
            "the conversation filled the model's context window",
        )),
        // This used to keep an unknown reason by name, as a normal stop, so
        // a new reason wouldn't break anything. But then a new kind of
        // failure would pass for an answer. pi-ai and llm-deepseek
        // (`translate.ts:95`) fail the call instead, and so does Karaka now.
        _ => Err(ModelError::new(
            ErrorKind::Malformed,
            format!("Anthropic stopped for a reason Karaka doesn't know: {reason}"),
        )),
    }
}

/// The error for a refusal, with what `stop_details` said about it. pi-ai
/// uses the explanation as the message, when there is one.
fn refusal(details: Option<StopDetails>) -> ModelError {
    let refusal = details.map(|details| Refusal {
        category: details.category.filter(|category| !category.is_empty()),
        explanation: details.explanation.filter(|text| !text.is_empty()),
    });
    let message = refusal
        .as_ref()
        .and_then(|refusal| refusal.explanation.clone())
        .unwrap_or_else(|| "Anthropic gave no explanation".into());
    let mut error = ModelError::new(ErrorKind::Refused, message);
    error.refusal = refusal;
    error
}

/// The kind of failure behind an error from Anthropic: from its type, its
/// message, and the HTTP status when it came as a failed response. `kind`
/// and `status` are `None` when Anthropic didn't send them.
///
/// The checks run in llm-deepseek's order (`transport.ts:28-35`), which
/// matters where two could match. An account out of credit can come back
/// as 429, but is no rate limit; a too-long prompt is also an invalid
/// request, but compaction can fix it.
pub(crate) fn classify(kind: Option<&str>, message: &str, status: Option<u16>) -> ErrorKind {
    let kind = kind.unwrap_or_default();
    // The type is searched too: `request_too_large` is one of pi-ai's
    // overflow patterns, since a shorter conversation is also a smaller one.
    let detail = format!("{kind} {message}");
    if matches!(status, Some(401 | 403))
        || matches!(kind, "authentication_error" | "permission_error")
    {
        ErrorKind::Auth
    } else if status == Some(402) || kind == "billing_error" || is_out_of_quota(&detail) {
        ErrorKind::Quota
    } else if status == Some(429) || kind == "rate_limit_error" {
        ErrorKind::RateLimited
    } else if is_context_overflow(&detail) {
        ErrorKind::ContextWindowExceeded
    } else if matches!(status, Some(400 | 413))
        || matches!(
            kind,
            "invalid_request_error" | "not_found_error" | "request_too_large"
        )
    {
        ErrorKind::InvalidRequest
    } else if status.is_none_or(|status| status >= 500)
        || matches!(kind, "api_error" | "overloaded_error")
    {
        // A 5xx, or an error in the stream of a type that's new to us,
        // which dsh also counts as a server error.
        ErrorKind::Unavailable
    } else {
        // 404 and every other status. dsh calls these `HTTP_<status>` and
        // doesn't retry them.
        ErrorKind::InvalidRequest
    }
}

/// A stream that broke Anthropic's own rules, which llm-deepseek calls
/// malformed (`translate.ts:134`).
fn protocol(what: &str) -> ModelError {
    ModelError::new(
        ErrorKind::Malformed,
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
    /// Sent with a refusal, and may be `null` even then.
    stop_details: Option<StopDetails>,
}

#[derive(Deserialize)]
struct StopDetails {
    category: Option<String>,
    explanation: Option<String>,
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

impl WireError {
    /// The failure this error means. `status` is the HTTP status if it came
    /// as a failed response, and `None` if it came in the stream.
    pub(crate) fn into_model_error(self, status: Option<u16>) -> ModelError {
        ModelError::new(
            classify(Some(&self.kind), &self.message, status),
            self.message,
        )
    }
}

#[cfg(test)]
mod tests {
    use karaka_model::{Block, ErrorKind, Event, ModelError, Refusal, StopReason, ToolCall, Usage};

    use super::{Decoder, classify};

    /// Feeds every event and keeps what comes out.
    fn decode(events: &[&str]) -> Result<Vec<Event>, ModelError> {
        let mut decoder = Decoder::default();
        let mut out = Vec::new();
        for event in events {
            out.extend(decoder.feed(event)?);
        }
        Ok(out)
    }

    /// Feeds events until one fails, and gives back what came out before
    /// the failure, and the failure. As a stream does, a caller sees both.
    fn decode_until_error(events: &[&str]) -> (Vec<Event>, ModelError) {
        let mut decoder = Decoder::default();
        let mut out = Vec::new();
        for event in events {
            match decoder.feed(event) {
                Ok(more) => out.extend(more),
                Err(error) => return (out, error),
            }
        }
        panic!("expected a failure, got {out:?}");
    }

    /// The stop reason `reason`, with `details` as its `stop_details`.
    fn stop(reason: &str, details: &str) -> String {
        format!(
            r#"{{"type":"message_delta","delta":{{"stop_reason":"{reason}","stop_sequence":null,"stop_details":{details}}},"usage":{{"output_tokens":2}}}}"#
        )
    }

    const START: &str = r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":20,"output_tokens":1,"cache_read_input_tokens":100,"cache_creation_input_tokens":0}}}"#;
    const TEXT_START: &str =
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
    const HEL: &str =
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#;
    const BLOCK_STOP: &str = r#"{"type":"content_block_stop","index":0}"#;
    const MESSAGE_STOP: &str = r#"{"type":"message_stop"}"#;

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
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":"Found it."}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            &stop("end_turn", "null"),
            MESSAGE_STOP,
        ])
        .expect("the stream decodes");

        let [Event::Block(Block::Text(text)), Event::Done { stop, .. }] = events.as_slice() else {
            panic!("expected the text block, then Done, got {events:?}");
        };
        assert_eq!(text, "Found it.");
        assert_eq!(*stop, StopReason::EndTurn);
    }

    #[test]
    fn a_refusal_fails_with_its_stop_details() {
        let (events, error) = decode_until_error(&[
            START,
            &stop(
                "refusal",
                r#"{"type":"refusal","category":"cyber","explanation":"That's not something I can help with."}"#,
            ),
            MESSAGE_STOP,
        ]);

        assert_eq!(events, []);
        assert_eq!(error.kind, ErrorKind::Refused);
        assert!(!error.kind.is_retryable());
        assert_eq!(error.message, "That's not something I can help with.");
        assert_eq!(
            error.refusal,
            Some(Refusal {
                category: Some("cyber".into()),
                explanation: Some("That's not something I can help with.".into()),
            })
        );
    }

    /// The text that streamed before the refusal has gone out, but the call
    /// ends in an `Err`, so nothing takes that text for the answer.
    #[test]
    fn a_refusal_after_some_text_fails_after_the_text() {
        let (events, error) = decode_until_error(&[
            START,
            TEXT_START,
            HEL,
            BLOCK_STOP,
            &stop("refusal", "null"),
            MESSAGE_STOP,
        ]);

        assert_eq!(
            events,
            [
                Event::TextDelta("Hel".into()),
                Event::Block(Block::Text("Hel".into())),
            ]
        );
        assert_eq!(error.kind, ErrorKind::Refused);
        // `stop_details` can be `null` even on a refusal.
        assert_eq!(error.refusal, None);
        assert_eq!(error.message, "Anthropic gave no explanation");
    }

    #[test]
    fn a_sensitive_stop_is_a_refusal() {
        let (_, error) = decode_until_error(&[
            START,
            TEXT_START,
            HEL,
            BLOCK_STOP,
            &stop("sensitive", "null"),
        ]);
        assert_eq!(error.kind, ErrorKind::Refused);
    }

    #[test]
    fn a_full_context_window_is_an_overflow() {
        let (_, error) = decode_until_error(&[
            START,
            TEXT_START,
            HEL,
            BLOCK_STOP,
            &stop("model_context_window_exceeded", "null"),
        ]);
        assert_eq!(error.kind, ErrorKind::ContextWindowExceeded);
        assert!(!error.kind.is_retryable());
    }

    #[test]
    fn an_unknown_stop_reason_is_malformed() {
        let (_, error) =
            decode_until_error(&[START, TEXT_START, HEL, BLOCK_STOP, &stop("mystery", "null")]);
        assert_eq!(error.kind, ErrorKind::Malformed);
        assert!(error.message.contains("mystery"), "{error}");
    }

    #[test]
    fn a_stop_sequence_is_a_normal_stop() {
        let events = decode(&[
            START,
            TEXT_START,
            HEL,
            BLOCK_STOP,
            &stop("stop_sequence", "null"),
            MESSAGE_STOP,
        ])
        .expect("the stream decodes");
        assert!(matches!(
            events.last(),
            Some(Event::Done {
                stop: StopReason::EndTurn,
                ..
            })
        ));
    }

    #[test]
    fn a_paused_turn_is_not_a_failure() {
        let events = decode(&[
            START,
            TEXT_START,
            HEL,
            BLOCK_STOP,
            &stop("pause_turn", "null"),
            MESSAGE_STOP,
        ])
        .expect("the stream decodes");
        assert!(matches!(
            events.last(),
            Some(Event::Done {
                stop: StopReason::Paused,
                ..
            })
        ));
    }

    /// dsh's "classifies a completed stop with no content as an
    /// EMPTY_RESPONSE error" (`llm-pi-ai/tests/convert.spec.ts`).
    #[test]
    fn a_completed_stop_with_no_content_is_an_empty_response() {
        let (_, error) = decode_until_error(&[START, &stop("end_turn", "null"), MESSAGE_STOP]);
        assert_eq!(error.kind, ErrorKind::EmptyResponse);
        assert!(error.kind.is_retryable());
    }

    /// dsh's "keeps a thinking-only stop successful (any block counts as
    /// content)" (`llm-pi-ai/tests/convert.spec.ts`).
    #[test]
    fn a_thinking_only_stop_stays_successful() {
        let events = decode(&[
            START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":"sig"}}"#,
            BLOCK_STOP,
            &stop("end_turn", "null"),
            MESSAGE_STOP,
        ])
        .expect("the stream decodes");

        assert_eq!(
            events[0],
            Event::Block(Block::Reasoning {
                text: String::new(),
                replay: Some("sig".into()),
            })
        );
        assert!(matches!(
            events[1],
            Event::Done {
                stop: StopReason::EndTurn,
                ..
            }
        ));
    }

    /// dsh's assembler drops "the tool calls a max-tokens finish drops"
    /// (`llm/tests/assembler.spec.ts`), and keeps what came before them.
    #[test]
    fn a_max_tokens_finish_drops_a_cut_off_tool_call() {
        let events = decode(&[
            START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"search"}}"#,
            BLOCK_STOP,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":"Looking."}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"query\": \"wea"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            &stop("max_tokens", "null"),
            MESSAGE_STOP,
        ])
        .expect("the stream decodes");

        assert_eq!(
            events,
            [
                Event::ReasoningDelta("search".into()),
                Event::Block(Block::Reasoning {
                    text: "search".into(),
                    replay: Some(String::new()),
                }),
                Event::Block(Block::Text("Looking.".into())),
                Event::Done {
                    stop: StopReason::MaxTokens,
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 2,
                        cache_read_tokens: 100,
                        cache_write_tokens: 0,
                    },
                },
            ]
        );
    }

    /// A tool call, and the text after it, wait for the stop reason; then
    /// both come out in order.
    #[test]
    fn tool_calls_and_what_follows_them_wait_for_the_stop_reason() {
        let mut decoder = Decoder::default();
        let mut feed = |data: &str| decoder.feed(data).expect("the event decodes");

        feed(START);
        feed(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"clock","input":{}}}"#,
        );
        assert_eq!(feed(BLOCK_STOP), [], "the tool call went out early");
        feed(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        );
        // The text's delta still goes out at once.
        assert_eq!(
            feed(
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Done."}}"#
            ),
            [Event::TextDelta("Done.".into())]
        );
        assert_eq!(feed(r#"{"type":"content_block_stop","index":1}"#), []);
        assert_eq!(feed(&stop("tool_use", "null")), []);

        let events = feed(MESSAGE_STOP);
        assert_eq!(
            events[..2],
            [
                Event::Block(Block::ToolCall(ToolCall {
                    id: "toolu_1".into(),
                    name: "clock".into(),
                    arguments: "{}".into(),
                })),
                Event::Block(Block::Text("Done.".into())),
            ]
        );
        assert!(matches!(
            events[2],
            Event::Done {
                stop: StopReason::ToolCalls,
                ..
            }
        ));
    }

    #[test]
    fn a_stream_without_a_stop_reason_is_malformed() {
        let (_, error) = decode_until_error(&[START, TEXT_START, HEL, BLOCK_STOP, MESSAGE_STOP]);
        assert_eq!(error.kind, ErrorKind::Malformed);
    }

    /// dsh's "classifies only context-capacity HTTP 400 details as context
    /// overflow", for Anthropic's own wording.
    #[test]
    fn only_a_too_long_prompt_is_an_overflow() {
        let too_long = "prompt is too long: 213462 tokens > 200000 maximum";
        assert_eq!(
            classify(Some("invalid_request_error"), too_long, Some(400)),
            ErrorKind::ContextWindowExceeded
        );
        let other = "max_tokens: 200000 > 128000, which is the maximum allowed number of output tokens for claude-opus-5-5";
        assert_eq!(
            classify(Some("invalid_request_error"), other, Some(400)),
            ErrorKind::InvalidRequest
        );
        // pi-ai counts the request's size in bytes as an overflow too.
        assert_eq!(
            classify(
                Some("request_too_large"),
                "Request exceeds the maximum size",
                Some(413)
            ),
            ErrorKind::ContextWindowExceeded
        );
    }

    #[test]
    fn statuses_and_types_map_as_in_dsh() {
        let cases = [
            (None, Some(401), ErrorKind::Auth),
            (Some("permission_error"), Some(403), ErrorKind::Auth),
            (None, Some(402), ErrorKind::Quota),
            (Some("billing_error"), Some(400), ErrorKind::Quota),
            (Some("rate_limit_error"), Some(429), ErrorKind::RateLimited),
            (
                Some("not_found_error"),
                Some(404),
                ErrorKind::InvalidRequest,
            ),
            // dsh's `HTTP_404` and `HTTP_418`, which it doesn't retry.
            (None, Some(404), ErrorKind::InvalidRequest),
            (None, Some(418), ErrorKind::InvalidRequest),
            (Some("overloaded_error"), Some(529), ErrorKind::Unavailable),
            (None, Some(503), ErrorKind::Unavailable),
            // In the stream, with no status.
            (Some("overloaded_error"), None, ErrorKind::Unavailable),
            (Some("some_new_error"), None, ErrorKind::Unavailable),
        ];
        for (kind, status, expected) in cases {
            assert_eq!(classify(kind, "", status), expected, "{kind:?} {status:?}");
        }
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
