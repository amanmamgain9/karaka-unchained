//! Anthropic's streamed events, turned into [`Event`]s.
//!
//! The [`Decoder`] only sees the JSON of each event, one at a time. Reading
//! those out of the HTTP body is a separate job, so everything here can be
//! tested without a network.
//!
//! DeepSeek sends the same events. Its profile reads them as dsh's
//! `llm-deepseek/src/translate.ts` does, which is stricter: a kind of block,
//! delta or stop reason it doesn't know fails the call instead of being
//! skipped, and so do events out of order and tool arguments that aren't a
//! JSON object.

use karaka_model::{
    Block, ErrorKind, Event, ModelError, Refusal, StopReason, ToolCall, Usage, is_context_overflow,
    is_out_of_quota,
};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::Profile;
use crate::deepseek;
use crate::request::json_object;

/// Assembles one streamed response.
///
/// Anthropic sends one content block at a time: start, deltas, stop. So the
/// decoder holds the block being written, and only the finished blocks that
/// must wait for the stop reason.
#[derive(Default)]
pub(crate) struct Decoder {
    profile: Profile,
    /// The model the request asked for. The DeepSeek profile records it
    /// beside each signature, so the signature goes back only to that model
    /// (`translate.ts:161`, `replay.ts:63`).
    model: String,
    /// Whether `message_start` has arrived. Only the DeepSeek profile
    /// checks the order of events.
    started: bool,
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
        /// `None` until a signature arrives. Anthropic always sends one;
        /// DeepSeek may not, and then there's nothing to send back.
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        /// The arguments if no JSON pieces arrive: `{}` for Anthropic, and
        /// the start's own `input` for DeepSeek (`translate.ts:56,140`).
        initial: String,
        json: String,
    },
    /// A block Karaka doesn't use, such as a server tool's. Its deltas and
    /// its stop are ignored.
    Skipped,
}

impl Decoder {
    /// A decoder for the DeepSeek profile, for an answer from `model`.
    pub(crate) fn deepseek(model: impl Into<String>) -> Self {
        Self {
            profile: Profile::DeepSeek,
            model: model.into(),
            ..Self::default()
        }
    }

    pub(crate) fn profile(&self) -> Profile {
        self.profile
    }

    /// Reads one event's JSON.
    ///
    /// Returns a list because `message_stop` releases every held block and
    /// then [`Event::Done`] at once. Every other event gives one event or
    /// none, and an empty `Vec` costs no allocation.
    ///
    /// An error event, a stop reason that isn't a finished answer, or JSON
    /// that doesn't fit ends the call with an `Err`.
    pub(crate) fn feed(&mut self, data: &str) -> Result<Vec<Event>, ModelError> {
        let deepseek = self.profile == Profile::DeepSeek;
        // DeepSeek's error events may lack a type or a message, which the
        // strict reading below would call unreadable. dsh reads whatever
        // is there (`transport.ts:22-27`), and so does this.
        if deepseek
            && let Ok(event) = serde_json::from_str::<deepseek::ErrorEvent>(data)
            && event.kind == "error"
        {
            return Err(event.error.into_model_error(None));
        }
        // dsh calls a stream it can't read malformed
        // (`llm-deepseek/src/sse.ts:18-19`), and so doesn't retry it.
        let event: WireEvent = serde_json::from_str(data).map_err(|error| {
            ModelError::new(
                ErrorKind::Malformed,
                format!(
                    "{} sent an event that couldn't be read: {error}",
                    self.profile.name()
                ),
            )
        })?;
        if deepseek {
            self.check_order(&event)?;
        }

        Ok(match event {
            WireEvent::MessageStart { message } => {
                self.add_usage(message.usage);
                Vec::new()
            }
            WireEvent::ContentBlockStart { content_block } => {
                let open = self.start(content_block, data)?;
                let events = self.initial_delta(&open).into_iter().collect();
                self.open = Some(open);
                events
            }
            WireEvent::ContentBlockDelta { delta } => self.delta(delta)?.into_iter().collect(),
            WireEvent::ContentBlockStop => {
                let open = self
                    .open
                    .take()
                    .ok_or_else(|| protocol(self.profile, "a block stopped"))?;
                match self.finish(open) {
                    Some(block) => self.finished(block),
                    None => Vec::new(),
                }
            }
            WireEvent::MessageDelta { delta, usage } => {
                self.add_usage(usage);
                if let Some(reason) = delta.stop_reason {
                    self.stop = Some(match self.profile {
                        Profile::Anthropic => stop_reason(&reason, delta.stop_details)?,
                        Profile::DeepSeek => deepseek_stop_reason(&reason)?,
                    });
                }
                Vec::new()
            }
            WireEvent::MessageStop => self.done()?,
            WireEvent::Error { error } => return Err(error.into_model_error(None)),
            WireEvent::Ping | WireEvent::Other => Vec::new(),
        })
    }

    /// The DeepSeek profile's checks on the order of events
    /// (`translate.ts:110-123,148`). Anthropic's own profile trusts the
    /// order, as it always has.
    fn check_order(&mut self, event: &WireEvent) -> Result<(), ModelError> {
        let profile = self.profile;
        let fail = |detail| Err(broken(profile, detail));
        match event {
            WireEvent::MessageStart { .. } if self.started => {
                fail("a second message_start arrived")
            }
            WireEvent::MessageStart { .. } => {
                self.started = true;
                Ok(())
            }
            // dsh skips event types it doesn't know, wherever they come.
            WireEvent::Ping | WireEvent::Other | WireEvent::Error { .. } => Ok(()),
            _ if !self.started => fail("an event arrived before message_start"),
            WireEvent::ContentBlockStart { .. } if self.stop.is_some() => {
                fail("a block started after the stop reason")
            }
            WireEvent::MessageStop if self.open.is_some() => {
                fail("the message stopped with a block still open")
            }
            _ => Ok(()),
        }
    }

    /// The block a `content_block_start` opens. `data` is the event's JSON,
    /// for the DeepSeek profile to read a tool's starting input from.
    fn start(&self, block: StartBlock, data: &str) -> Result<Open, ModelError> {
        Ok(match block {
            StartBlock::Text { text } => Open::Text(text),
            StartBlock::Thinking {
                thinking,
                signature,
            } => Open::Thinking {
                text: thinking,
                signature,
            },
            StartBlock::ToolUse { id, name } => {
                let initial = match self.profile {
                    Profile::Anthropic => "{}".to_owned(),
                    Profile::DeepSeek => {
                        // `translate.ts:56-57`.
                        if id.is_empty() || name.is_empty() {
                            return Err(broken(self.profile, "a tool call has no id or no name"));
                        }
                        tool_input(data).ok_or_else(|| {
                            broken(self.profile, "a tool call's input isn't a JSON object")
                        })?
                    }
                };
                Open::ToolUse {
                    id,
                    name,
                    initial,
                    json: String::new(),
                }
            }
            StartBlock::Other => match self.profile {
                Profile::Anthropic => Open::Skipped,
                // dsh calls this unsupported content (`translate.ts:60`).
                // Trying again would get the same kind of block.
                Profile::DeepSeek => {
                    return Err(ModelError::new(
                        ErrorKind::Malformed,
                        "DeepSeek sent a kind of block Karaka doesn't support",
                    ));
                }
            },
        })
    }

    /// Text that a DeepSeek block starts with, passed on as a delta, as dsh
    /// does (`translate.ts:127-128`). Anthropic starts every block empty.
    fn initial_delta(&self, open: &Open) -> Option<Event> {
        if self.profile != Profile::DeepSeek {
            return None;
        }
        match open {
            Open::Text(text) if !text.is_empty() => Some(Event::TextDelta(text.clone())),
            Open::Thinking { text, .. } if !text.is_empty() => {
                Some(Event::ReasoningDelta(text.clone()))
            }
            _ => None,
        }
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
        let provider = self.profile.name();
        let stop = self.stop.take().ok_or_else(|| {
            // llm-deepseek's `translate.ts:148`.
            ModelError::new(
                ErrorKind::Malformed,
                format!("{provider} finished without saying why it stopped"),
            )
        })?;
        // A finished answer with nothing in it would end the turn with
        // nothing to show or act on. dsh retries it instead
        // (`llm-pi-ai/src/stream.ts:96-107`, `llm-deepseek/src/translate.ts:149`).
        // Any block counts, so an answer that is only thinking is kept.
        if stop == StopReason::EndTurn && !self.any_block {
            return Err(ModelError::new(
                ErrorKind::EmptyResponse,
                format!("{provider} finished an answer with no content"),
            ));
        }
        // dsh refuses finished tool arguments that aren't a JSON object,
        // rather than run a tool on them (`translate.ts:151-158`). Ones cut
        // off by `max_tokens` are dropped below instead.
        if self.profile == Profile::DeepSeek && stop != StopReason::MaxTokens {
            let broken_call = self.held.iter().any(|block| {
                matches!(block, Block::ToolCall(call) if json_object(&call.arguments).is_none())
            });
            if broken_call {
                return Err(broken(
                    self.profile,
                    "a tool call's arguments aren't a JSON object",
                ));
            }
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
        let profile = self.profile;
        let open = self
            .open
            .as_mut()
            .ok_or_else(|| protocol(profile, "a delta arrived"))?;
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
                signature.get_or_insert_default().push_str(&more);
                None
            }
            // Tool arguments arrive as pieces of JSON that only parse once
            // they're all there, so they aren't passed on one by one.
            (Open::ToolUse { json, .. }, Delta::InputJson { partial_json }) => {
                json.push_str(&partial_json);
                None
            }
            (Open::Skipped, _) => None,
            // A new kind of delta. DeepSeek's profile refuses it
            // (`translate.ts:87`), below.
            (_, Delta::Other) if profile == Profile::Anthropic => None,
            _ => {
                return Err(broken(profile, "a delta of the wrong kind arrived"));
            }
        })
    }

    fn add_usage(&mut self, wire: WireUsage) {
        // Anthropic's counts are running totals, so a later value replaces an
        // earlier one rather than adding to it. DeepSeek's are too
        // (`translate.ts:34-43`).
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

    /// A finished block, or `None` for one Karaka skips.
    fn finish(&self, open: Open) -> Option<Block> {
        Some(match open {
            Open::Text(text) => Block::Text(text),
            // Kept even when the text is empty, as it is unless thinking is set
            // to be shown: the signature still has to go back next turn.
            Open::Thinking { text, signature } => Block::Reasoning {
                text,
                replay: match self.profile {
                    Profile::Anthropic => Some(signature.unwrap_or_default()),
                    // With no signature there's nothing to send back, so
                    // no model to record either (`translate.ts:53`).
                    Profile::DeepSeek => {
                        signature.map(|signature| deepseek::replay(&self.model, &signature))
                    }
                },
            },
            Open::ToolUse {
                id,
                name,
                initial,
                json,
            } => Block::ToolCall(ToolCall {
                id,
                name,
                // A tool without arguments may send no JSON at all.
                arguments: if json.is_empty() { initial } else { json },
            }),
            Open::Skipped => return None,
        })
    }
}

/// The `input` of the tool call that `data`, a `content_block_start` event,
/// starts, as the server wrote it. `None` unless it is a JSON object.
fn tool_input(data: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Start<'a> {
        #[serde(borrow)]
        content_block: StartedTool<'a>,
    }
    #[derive(Deserialize)]
    struct StartedTool<'a> {
        #[serde(borrow)]
        input: &'a RawValue,
    }
    let start: Start<'_> = serde_json::from_str(data).ok()?;
    let input = start.content_block.input.get();
    input.starts_with('{').then(|| input.to_owned())
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

/// What a stop reason means for DeepSeek. dsh knows only these four, and
/// calls any other malformed (`translate.ts:90-97`), refusals and
/// `pause_turn` among them.
fn deepseek_stop_reason(reason: &str) -> Result<StopReason, ModelError> {
    match reason {
        "end_turn" | "stop_sequence" => Ok(StopReason::EndTurn),
        "tool_use" => Ok(StopReason::ToolCalls),
        "max_tokens" => Ok(StopReason::MaxTokens),
        _ => Err(ModelError::new(
            ErrorKind::Malformed,
            format!("DeepSeek stopped for a reason Karaka doesn't know: {reason}"),
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

/// A stream that broke the provider's own rules, which llm-deepseek calls
/// malformed (`translate.ts:25-27`).
fn broken(profile: Profile, detail: &str) -> ModelError {
    ModelError::new(
        ErrorKind::Malformed,
        format!("{}'s stream broke its own rules: {detail}", profile.name()),
    )
}

/// A delta or a stop with no block open (`translate.ts:134`).
fn protocol(profile: Profile, what: &str) -> ModelError {
    broken(profile, &format!("{what} with no block open"))
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
        signature: Option<String>,
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

    // The DeepSeek profile. The cases come from dsh's
    // `llm-deepseek/tests/stream.spec.ts`, with its fixtures
    // (`tests/helpers.ts:22-29`).

    /// dsh's test model.
    const DS_MODEL: &str = "deepseek-v4-flash";
    const DS_START: &str = r#"{"type":"message_start","message":{"id":"msg_1","model":"deepseek-v4-flash","usage":{"input_tokens":12,"output_tokens":1}}}"#;
    const DS_TEXT_START: &str =
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
    const DS_TEXT: &str = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello 世界"}}"#;

    fn ds_stop(reason: &str) -> String {
        format!(
            r#"{{"type":"message_delta","delta":{{"stop_reason":"{reason}"}},"usage":{{"output_tokens":5}}}}"#
        )
    }

    fn deepseek_decode(events: &[&str]) -> Result<Vec<Event>, ModelError> {
        let mut decoder = Decoder::deepseek(DS_MODEL);
        let mut out = Vec::new();
        for event in events {
            out.extend(decoder.feed(event)?);
        }
        Ok(out)
    }

    fn deepseek_error(events: &[&str]) -> ModelError {
        deepseek_decode(events).expect_err("the call fails")
    }

    /// The blocks among `events`, without the deltas and `Done`.
    fn blocks(events: &[Event]) -> Vec<Block> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Block(block) => Some(block.clone()),
                _ => None,
            })
            .collect()
    }

    /// dsh's "emits first-seen blocks, raw tool JSON and cumulative
    /// disjoint usage before finish".
    #[test]
    fn deepseek_signatures_are_bound_to_the_requested_model() {
        let events = deepseek_decode(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":1,"cache_read_input_tokens":30,"cache_creation_input_tokens":7}}}"#,
            r#"{"type":"content_block_start","index":4,"content_block":{"type":"thinking","thinking":"first","signature":""}}"#,
            r#"{"type":"content_block_delta","index":4,"delta":{"type":"thinking_delta","thinking":" thought"}}"#,
            r#"{"type":"content_block_delta","index":4,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"{"type":"content_block_delta","index":4,"delta":{"type":"signature_delta","signature":"nature"}}"#,
            r#"{"type":"content_block_stop","index":4}"#,
            r#"{"type":"content_block_start","index":9,"content_block":{"type":"tool_use","id":"call_1","name":"read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":9,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"{"type":"content_block_delta","index":9,"delta":{"type":"input_json_delta","partial_json":" \"a\"}"}}"#,
            r#"{"type":"content_block_stop","index":9}"#,
            r#"{"type":"message_delta","delta":{},"usage":{"output_tokens":3}}"#,
            &ds_stop("tool_use"),
            MESSAGE_STOP,
        ])
        .expect("the stream decodes");

        assert_eq!(
            events,
            [
                // The text the block started with comes first, as a delta.
                Event::ReasoningDelta("first".into()),
                Event::ReasoningDelta(" thought".into()),
                Event::Block(Block::Reasoning {
                    text: "first thought".into(),
                    replay: Some(crate::deepseek::replay(DS_MODEL, "signature")),
                }),
                Event::Block(Block::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: r#"{"path": "a"}"#.into(),
                })),
                Event::Done {
                    stop: StopReason::ToolCalls,
                    usage: Usage {
                        input_tokens: 12,
                        output_tokens: 5,
                        cache_read_tokens: 30,
                        cache_write_tokens: 7,
                    },
                },
            ]
        );
    }

    /// dsh's "streams text already present in the starting block".
    #[test]
    fn deepseek_text_in_the_starting_block_is_streamed() {
        let events = deepseek_decode(&[
            DS_START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"initial"}}"#,
            BLOCK_STOP,
            &ds_stop("end_turn"),
            MESSAGE_STOP,
        ])
        .expect("the stream decodes");
        assert_eq!(events[0], Event::TextDelta("initial".into()));
        assert_eq!(events[1], Event::Block(Block::Text("initial".into())));
    }

    /// dsh's "preserves initial tool input %j without JSON deltas".
    #[test]
    fn a_deepseek_tool_keeps_its_starting_input_without_json_pieces() {
        for input in ["{}", r#"{"key":"initial"}"#] {
            let start = format!(
                r#"{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"a","name":"read","input":{input}}}}}"#
            );
            let events = deepseek_decode(&[
                DS_START,
                &start,
                BLOCK_STOP,
                &ds_stop("tool_use"),
                MESSAGE_STOP,
            ])
            .expect("the stream decodes");
            let blocks = blocks(&events);
            let [Block::ToolCall(call)] = blocks.as_slice() else {
                panic!("expected one tool call, got {events:?}");
            };
            assert_eq!(call.arguments, input);
        }
    }

    /// dsh's "preserves signature-only thinking and prunes truncated tools
    /// with their replay entries". A thinking block with no signature at
    /// all keeps no replay.
    #[test]
    fn deepseek_signature_only_thinking_is_kept_and_a_cut_off_tool_dropped() {
        let events = deepseek_decode(&[
            DS_START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque"}}"#,
            BLOCK_STOP,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":"unsigned"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"a","name":"read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            &ds_stop("max_tokens"),
            MESSAGE_STOP,
        ])
        .expect("the stream decodes");
        assert_eq!(
            blocks(&events),
            [
                Block::Reasoning {
                    text: String::new(),
                    replay: Some(crate::deepseek::replay(DS_MODEL, "opaque")),
                },
                Block::Reasoning {
                    text: "unsigned".into(),
                    replay: None,
                },
            ]
        );
    }

    #[test]
    fn deepseek_end_turn_and_stop_sequence_are_normal_stops() {
        for reason in ["end_turn", "stop_sequence"] {
            let events = deepseek_decode(&[
                DS_START,
                r#"{"type":"future_event"}"#,
                DS_TEXT_START,
                DS_TEXT,
                BLOCK_STOP,
                &ds_stop(reason),
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
    }

    /// dsh's "rejects malformed event ordering or fields %#", for every
    /// case Karaka can tell apart.
    #[test]
    fn deepseek_streams_that_break_the_rules_are_malformed() {
        let negative_usage =
            r#"{"type":"message_delta","delta":{},"usage":{"input_tokens":-1}}"#.to_owned();
        let cases: Vec<Vec<String>> = vec![
            vec![DS_START.into(), DS_START.into()],
            vec![DS_TEXT_START.into()],
            vec![DS_START.into(), BLOCK_STOP.into()],
            vec![
                DS_START.into(),
                DS_TEXT_START.into(),
                BLOCK_STOP.into(),
                BLOCK_STOP.into(),
            ],
            vec![
                DS_START.into(),
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":2}}"#.into(),
            ],
            vec![
                DS_START.into(),
                DS_TEXT_START.into(),
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"bad"}}"#.into(),
            ],
            vec![DS_START.into(), ds_stop("mystery")],
            vec![DS_START.into(), MESSAGE_STOP.into()],
            vec![
                DS_START.into(),
                DS_TEXT_START.into(),
                ds_stop("end_turn"),
                MESSAGE_STOP.into(),
            ],
            vec![DS_START.into(), negative_usage],
            vec![DS_START.into(), ds_stop("end_turn"), DS_TEXT_START.into()],
            vec![
                DS_START.into(),
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"","name":"read","input":{}}}"#.into(),
            ],
            vec![
                DS_START.into(),
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"x","name":"read","input":[]}}"#.into(),
            ],
            // A delta of a kind nobody knows. Anthropic's profile skips it.
            vec![
                DS_START.into(),
                DS_TEXT_START.into(),
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"citations_delta"}}"#.into(),
            ],
        ];
        for events in cases {
            let events: Vec<&str> = events.iter().map(String::as_str).collect();
            let error = deepseek_error(&events);
            assert_eq!(error.kind, ErrorKind::Malformed, "{events:?}: {error}");
        }
    }

    /// dsh's "refuses completed non-object tool JSON %s without repairing
    /// it".
    #[test]
    fn deepseek_tool_arguments_that_arent_an_object_are_malformed() {
        for json in ["{", "[]"] {
            let delta = format!(
                r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"input_json_delta","partial_json":"{json}"}}}}"#
            );
            let error = deepseek_error(&[
                DS_START,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"a","name":"read","input":{}}}"#,
                &delta,
                BLOCK_STOP,
                &ds_stop("tool_use"),
                MESSAGE_STOP,
            ]);
            assert_eq!(error.kind, ErrorKind::Malformed, "{json}");
            assert!(error.message.contains("aren't a JSON object"), "{error}");
        }
    }

    /// dsh's "refuses unsupported response content, empty responses and
    /// premature EOF". The early end of the stream is the HTTP side's job.
    #[test]
    fn deepseek_refuses_blocks_it_doesnt_support_and_empty_answers() {
        let error = deepseek_error(&[
            DS_START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"x"}}"#,
        ]);
        assert_eq!(error.kind, ErrorKind::Malformed);
        assert!(!error.kind.is_retryable());

        let error = deepseek_error(&[DS_START, &ds_stop("end_turn"), MESSAGE_STOP]);
        assert_eq!(error.kind, ErrorKind::EmptyResponse);
        assert_eq!(error.message, "DeepSeek finished an answer with no content");
    }

    /// dsh knows only four stop reasons. A refusal or a paused turn, which
    /// Anthropic's profile reads, is malformed from DeepSeek.
    #[test]
    fn deepseek_knows_only_four_stop_reasons() {
        for reason in [
            "refusal",
            "pause_turn",
            "sensitive",
            "model_context_window_exceeded",
        ] {
            let error = deepseek_error(&[DS_START, &ds_stop(reason)]);
            assert_eq!(error.kind, ErrorKind::Malformed, "{reason}");
            assert_eq!(
                error.message,
                format!("DeepSeek stopped for a reason Karaka doesn't know: {reason}")
            );
        }
    }

    /// dsh's "normalizes in-band overloads", and an error with nothing in
    /// it, which dsh counts as a server error.
    #[test]
    fn deepseek_error_events_are_read_whatever_they_hold() {
        let error = deepseek_error(&[
            DS_START,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
        ]);
        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert_eq!(error.message, "busy");

        let error = deepseek_error(&[r#"{"type":"error","error":{}}"#]);
        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert_eq!(error.message, "DeepSeek's request failed (stream error)");

        let error = deepseek_error(&[r#"{"type":"error"}"#]);
        assert_eq!(error.kind, ErrorKind::Unavailable);
    }

    /// The same malformed stream from Anthropic still goes by Anthropic's
    /// looser rules.
    #[test]
    fn the_anthropic_profile_keeps_its_own_rules() {
        let events = decode(&[
            TEXT_START,
            HEL,
            BLOCK_STOP,
            &stop("end_turn", "null"),
            MESSAGE_STOP,
        ])
        .expect("Anthropic doesn't check for message_start");
        assert_eq!(events[0], Event::TextDelta("Hel".into()));
    }
}
