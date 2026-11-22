//! Anthropic's streamed events, turned into dsh's chunks ([`Event`]).
//!
//! The [`Decoder`] only sees the JSON of each event, one at a time. Reading
//! those out of the HTTP body is a separate job, so everything here can be
//! tested without a network.
//!
//! Anthropic's events map onto dsh's chunks one to one, as dsh's DeepSeek
//! adapter maps the same events (`llm-deepseek/src/translate.ts:103-166`)
//! and its pi-ai adapter maps pi-ai's (`llm-pi-ai/src/stream.ts:157-232`):
//!
//! - `content_block_start` is a `block-start`, `content_block_delta` a
//!   text, reasoning or tool-call delta, and `content_block_stop` a
//!   `block-end` with the finished block. Blocks are numbered from 0 in the
//!   order they start; a block Karaka skips gets no number.
//! - `message_stop` gives `usage`, then `finish`.
//! - A signature arrives as a `signature_delta`, which is no chunk: it goes
//!   into the reasoning block's `replay` at its end.
//!
//! Nothing is held back. A tool call cut off by `max_tokens` streams like
//! any other, and the assembler drops it once the finish says why the model
//! stopped, as dsh's does ("Truncated tool JSON is retained in the stream,
//! then pruned by the shared assembler", `translate.ts:150`).
//!
//! DeepSeek sends the same events. Its profile reads them as dsh's
//! `llm-deepseek/src/translate.ts` does, which is stricter: a kind of block,
//! delta or stop reason it doesn't know fails the call instead of being
//! skipped, and so do events out of order and tool arguments that aren't a
//! JSON object.

use karaka_model::{
    Block, BlockType, ErrorKind, Event, FinishReason, ModelError, Origin, Refusal, Replay,
    ToolCall, Usage, is_context_overflow, is_out_of_quota,
};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::Profile;
use crate::deepseek;
use crate::request::{self, json_object};

/// Turns one streamed response into chunks.
///
/// Anthropic sends one content block at a time: start, deltas, stop. So the
/// decoder holds only the block being written.
pub(crate) struct Decoder {
    profile: Profile,
    /// Where each signature in this answer comes from: the profile's API
    /// and provider, and the model the request asked for. Signatures go
    /// back only to that same model, as pi-ai and dsh's DeepSeek adapter
    /// send them (`llm-pi-ai/src/replay.ts:77-83`, `llm-deepseek/src/translate.ts:161`).
    origin: Origin,
    /// Whether `message_start` has arrived. Only the DeepSeek profile
    /// checks the order of events.
    started: bool,
    open: Option<Open>,
    /// The index the next block that starts gets.
    next_index: usize,
    /// The arguments of every tool call that has finished, which the
    /// DeepSeek profile checks once it knows why the model stopped.
    finished_calls: Vec<String>,
    /// Whether any block has finished. A normal stop with none is an empty
    /// response.
    any_block: bool,
    usage: Usage,
    stop: Option<FinishReason>,
}

/// A block that has started and not yet stopped.
struct Open {
    /// `None` for a block Karaka skips, which sends no chunks.
    index: Option<usize>,
    kind: OpenKind,
}

enum OpenKind {
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
    /// A decoder for the Anthropic profile, for an answer from `model`.
    pub(crate) fn anthropic(model: impl Into<String>) -> Self {
        Self::new(
            Profile::Anthropic,
            Origin::new(request::API, request::PROVIDER, model),
        )
    }

    /// A decoder for the DeepSeek profile, for an answer from `model`.
    pub(crate) fn deepseek(model: impl Into<String>) -> Self {
        Self::new(
            Profile::DeepSeek,
            Origin::new(deepseek::API, deepseek::PROVIDER, model),
        )
    }

    fn new(profile: Profile, origin: Origin) -> Self {
        Self {
            profile,
            origin,
            started: false,
            open: None,
            next_index: 0,
            finished_calls: Vec::new(),
            any_block: false,
            usage: Usage::default(),
            stop: None,
        }
    }

    pub(crate) fn profile(&self) -> Profile {
        self.profile
    }

    /// Reads one event's JSON, and gives the chunks it means: none, one,
    /// or a few, such as a block's start and its first text. An empty `Vec`
    /// costs no allocation.
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
                let kind = self.start(content_block, data)?;
                let open = Open {
                    index: (!matches!(kind, OpenKind::Skipped)).then(|| {
                        let index = self.next_index;
                        self.next_index += 1;
                        index
                    }),
                    kind,
                };
                let events = self.opening(&open);
                self.open = Some(open);
                events
            }
            WireEvent::ContentBlockDelta { delta } => self.delta(delta)?.into_iter().collect(),
            WireEvent::ContentBlockStop => {
                let open = self
                    .open
                    .take()
                    .ok_or_else(|| protocol(self.profile, "a block stopped"))?;
                self.finish(open).into_iter().collect()
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
    fn start(&self, block: StartBlock, data: &str) -> Result<OpenKind, ModelError> {
        Ok(match block {
            StartBlock::Text { text } => OpenKind::Text(text),
            StartBlock::Thinking {
                thinking,
                signature,
            } => OpenKind::Thinking {
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
                OpenKind::ToolUse {
                    id,
                    name,
                    initial,
                    json: String::new(),
                }
            }
            StartBlock::Other => match self.profile {
                Profile::Anthropic => OpenKind::Skipped,
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

    /// The chunks a block opens with: its `block-start`, and for DeepSeek
    /// what dsh sends with it (`translate.ts:124-131`): the text the block
    /// starts with, if any, or a tool call's id and name in an empty delta.
    /// Anthropic starts every text block empty, and pi-ai names a tool call
    /// in each of its deltas instead (`llm-pi-ai/src/stream.ts:181-201`).
    fn opening(&self, open: &Open) -> Vec<Event> {
        let Some(index) = open.index else {
            return Vec::new();
        };
        let block_type = match &open.kind {
            OpenKind::Text(_) => BlockType::Text,
            OpenKind::Thinking { .. } => BlockType::Reasoning,
            OpenKind::ToolUse { .. } => BlockType::ToolCall,
            OpenKind::Skipped => return Vec::new(),
        };
        let mut events = vec![Event::BlockStart { index, block_type }];
        if self.profile == Profile::DeepSeek {
            match &open.kind {
                OpenKind::Text(text) if !text.is_empty() => events.push(Event::TextDelta {
                    index,
                    text: text.clone(),
                }),
                OpenKind::Thinking { text, .. } if !text.is_empty() => {
                    events.push(Event::ReasoningDelta {
                        index,
                        text: text.clone(),
                    });
                }
                OpenKind::ToolUse { id, name, .. } => events.push(Event::ToolCallDelta {
                    index,
                    id: id.clone(),
                    name: Some(name.clone()),
                    arguments_delta: String::new(),
                }),
                _ => {}
            }
        }
        events
    }

    /// `usage`, then `finish`.
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
        if stop == FinishReason::Stop && !self.any_block {
            return Err(ModelError::new(
                ErrorKind::EmptyResponse,
                format!("{provider} finished an answer with no content"),
            ));
        }
        // dsh refuses finished tool arguments that aren't a JSON object,
        // rather than run a tool on them (`translate.ts:151-158`). Ones cut
        // off by `max_tokens` stream as they are, and the assembler drops
        // them.
        if self.profile == Profile::DeepSeek
            && stop != FinishReason::MaxTokens
            && self
                .finished_calls
                .iter()
                .any(|arguments| json_object(arguments).is_none())
        {
            return Err(broken(
                self.profile,
                "a tool call's arguments aren't a JSON object",
            ));
        }
        Ok(vec![
            Event::Usage { usage: self.usage },
            Event::Finish { reason: stop },
        ])
    }

    fn delta(&mut self, delta: Delta) -> Result<Option<Event>, ModelError> {
        let profile = self.profile;
        let open = self
            .open
            .as_mut()
            .ok_or_else(|| protocol(profile, "a delta arrived"))?;
        let index = open.index;
        let event = match (&mut open.kind, delta) {
            (OpenKind::Text(text), Delta::Text { text: more }) => {
                text.push_str(&more);
                Some(Event::TextDelta {
                    index: index.unwrap_or_default(),
                    text: more,
                })
            }
            (OpenKind::Thinking { text, .. }, Delta::Thinking { thinking: more }) => {
                text.push_str(&more);
                Some(Event::ReasoningDelta {
                    index: index.unwrap_or_default(),
                    text: more,
                })
            }
            (OpenKind::Thinking { signature, .. }, Delta::Signature { signature: more }) => {
                signature.get_or_insert_default().push_str(&more);
                None
            }
            // Each piece of the arguments goes on as it is, raw JSON text.
            // pi-ai names the call in each piece, and dsh's DeepSeek adapter
            // only in the first, at the block's start.
            (OpenKind::ToolUse { id, name, json, .. }, Delta::InputJson { partial_json }) => {
                json.push_str(&partial_json);
                Some(Event::ToolCallDelta {
                    index: index.unwrap_or_default(),
                    id: id.clone(),
                    name: (profile == Profile::Anthropic).then(|| name.clone()),
                    arguments_delta: partial_json,
                })
            }
            (OpenKind::Skipped, _) => None,
            // A new kind of delta. DeepSeek's profile refuses it
            // (`translate.ts:87`), below.
            (_, Delta::Other) if profile == Profile::Anthropic => None,
            _ => {
                return Err(broken(profile, "a delta of the wrong kind arrived"));
            }
        };
        Ok(event)
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

    /// The `block-end` for a block that stopped, or `None` for one Karaka
    /// skips.
    fn finish(&mut self, open: Open) -> Option<Event> {
        let index = open.index?;
        let block = match open.kind {
            OpenKind::Text(text) => Block::Text(text),
            // Kept even when the text is empty, as it is unless thinking is set
            // to be shown: the signature still has to go back next turn.
            OpenKind::Thinking { text, signature } => Block::Reasoning {
                text,
                replay: match self.profile {
                    Profile::Anthropic => Some(signature.unwrap_or_default()),
                    // With no signature there's nothing to send back, so
                    // no origin to record either (`translate.ts:53`).
                    Profile::DeepSeek => signature,
                }
                .map(|signature| Replay::new(self.origin.clone(), signature)),
            },
            OpenKind::ToolUse {
                id,
                name,
                initial,
                json,
            } => {
                // A tool without arguments may send no JSON at all.
                let arguments = if json.is_empty() { initial } else { json };
                self.finished_calls.push(arguments.clone());
                Block::ToolCall(ToolCall {
                    id,
                    name,
                    arguments,
                })
            }
            OpenKind::Skipped => return None,
        };
        self.any_block = true;
        Some(Event::BlockEnd { index, block })
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
fn stop_reason(reason: &str, details: Option<StopDetails>) -> Result<FinishReason, ModelError> {
    match reason {
        // Karaka sends no stop sequences, so `stop_sequence` shouldn't come.
        // If it does, the answer ended where the request asked.
        "end_turn" | "stop_sequence" => Ok(FinishReason::Stop),
        "tool_use" => Ok(FinishReason::ToolCalls),
        "max_tokens" => Ok(FinishReason::MaxTokens),
        // A server-tools turn that Anthropic paused and will resume. Not a
        // failure, so it isn't an error. pi-ai calls it a normal stop; Karaka
        // keeps it apart, because the turn isn't finished.
        "pause_turn" => Ok(FinishReason::Paused),
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
fn deepseek_stop_reason(reason: &str) -> Result<FinishReason, ModelError> {
    match reason {
        "end_turn" | "stop_sequence" => Ok(FinishReason::Stop),
        "tool_use" => Ok(FinishReason::ToolCalls),
        "max_tokens" => Ok(FinishReason::MaxTokens),
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
    use karaka_model::{
        Block, BlockAssembler, BlockType, ErrorKind, Event, FinishReason, ModelError, Origin,
        Refusal, Replay, ToolCall, Usage,
    };

    use super::{Decoder, classify};

    /// The model every Anthropic test asks for.
    const MODEL: &str = "claude-opus-5-5";

    /// The replay a signature from Anthropic's `MODEL` gets.
    fn signed(signature: &str) -> Option<Replay> {
        Some(Replay::new(
            Origin::new("anthropic-messages", "anthropic", MODEL),
            signature,
        ))
    }

    /// Feeds every event and keeps what comes out.
    fn decode(events: &[&str]) -> Result<Vec<Event>, ModelError> {
        let mut decoder = Decoder::anthropic(MODEL);
        let mut out = Vec::new();
        for event in events {
            out.extend(decoder.feed(event)?);
        }
        Ok(out)
    }

    /// Feeds events until one fails, and gives back what came out before
    /// the failure, and the failure. As a stream does, a caller sees both.
    fn decode_until_error(events: &[&str]) -> (Vec<Event>, ModelError) {
        let mut decoder = Decoder::anthropic(MODEL);
        let mut out = Vec::new();
        for event in events {
            match decoder.feed(event) {
                Ok(more) => out.extend(more),
                Err(error) => return (out, error),
            }
        }
        panic!("expected a failure, got {out:?}");
    }

    /// The blocks among `events`, from their ends.
    fn blocks(events: &[Event]) -> Vec<Block> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::BlockEnd { block, .. } => Some(block.clone()),
                _ => None,
            })
            .collect()
    }

    /// Why the stream finished, from its last chunk.
    fn finish(events: &[Event]) -> Option<FinishReason> {
        match events.last() {
            Some(Event::Finish { reason }) => Some(*reason),
            _ => None,
        }
    }

    /// The answer as the assembler puts it together.
    fn assembled(events: &[Event]) -> Vec<Block> {
        let mut answer = BlockAssembler::new();
        for event in events {
            answer.push(event);
        }
        answer.blocks()
    }

    fn text_delta(index: usize, text: &str) -> Event {
        Event::TextDelta {
            index,
            text: text.into(),
        }
    }

    fn start(index: usize, block_type: BlockType) -> Event {
        Event::BlockStart { index, block_type }
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
                start(0, BlockType::Text),
                text_delta(0, "Hel"),
                text_delta(0, "lo"),
                Event::BlockEnd {
                    index: 0,
                    block: Block::Text("Hello".into())
                },
                Event::Usage {
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 5,
                        cache_read_tokens: 100,
                        cache_write_tokens: 0,
                    }
                },
                Event::Finish {
                    reason: FinishReason::Stop
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

        // The arguments stream as raw JSON pieces, each naming the call,
        // as pi-ai's do (`llm-pi-ai/src/stream.ts:189-201`).
        assert_eq!(
            events[..8],
            [
                start(0, BlockType::Reasoning),
                Event::ReasoningDelta {
                    index: 0,
                    text: "check the clock".into()
                },
                Event::BlockEnd {
                    index: 0,
                    block: Block::Reasoning {
                        text: "check the clock".into(),
                        replay: signed("sig"),
                    }
                },
                start(1, BlockType::ToolCall),
                Event::ToolCallDelta {
                    index: 1,
                    id: "toolu_1".into(),
                    name: Some("clock".into()),
                    arguments_delta: r#"{"zone": "#.into()
                },
                Event::ToolCallDelta {
                    index: 1,
                    id: "toolu_1".into(),
                    name: Some("clock".into()),
                    arguments_delta: r#""UTC"}"#.into()
                },
                Event::BlockEnd {
                    index: 1,
                    block: Block::ToolCall(ToolCall {
                        id: "toolu_1".into(),
                        name: "clock".into(),
                        arguments: r#"{"zone": "UTC"}"#.into(),
                    })
                },
                Event::Usage {
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 30,
                        cache_read_tokens: 100,
                        cache_write_tokens: 0,
                    }
                },
            ]
        );
        assert_eq!(finish(&events), Some(FinishReason::ToolCalls));
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

        // The skipped block gets no index, so the text is block 0.
        assert_eq!(
            events[..2],
            [
                start(0, BlockType::Text),
                Event::BlockEnd {
                    index: 0,
                    block: Block::Text("Found it.".into())
                },
            ]
        );
        assert_eq!(finish(&events), Some(FinishReason::Stop));
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
                start(0, BlockType::Text),
                text_delta(0, "Hel"),
                Event::BlockEnd {
                    index: 0,
                    block: Block::Text("Hel".into())
                },
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
        assert_eq!(finish(&events), Some(FinishReason::Stop));
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
        assert_eq!(finish(&events), Some(FinishReason::Paused));
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
            blocks(&events),
            [Block::Reasoning {
                text: String::new(),
                replay: signed("sig"),
            }]
        );
        assert_eq!(finish(&events), Some(FinishReason::Stop));
    }

    /// A tool call cut off by `max_tokens` still streams, as in dsh
    /// ("Truncated tool JSON is retained in the stream", `translate.ts:150`),
    /// and the assembler drops it, keeping what came before it
    /// (`llm/tests/assembler.spec.ts`).
    #[test]
    fn a_max_tokens_finish_streams_a_cut_off_tool_call_for_the_assembler_to_drop() {
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

        let cut_off = Block::ToolCall(ToolCall {
            id: "toolu_1".into(),
            name: "search".into(),
            arguments: r#"{"query": "wea"#.into(),
        });
        let reasoning = Block::Reasoning {
            text: "search".into(),
            replay: signed(""),
        };
        let looking = Block::Text("Looking.".into());
        assert_eq!(
            blocks(&events),
            [reasoning.clone(), looking.clone(), cut_off]
        );
        assert_eq!(finish(&events), Some(FinishReason::MaxTokens));
        assert_eq!(assembled(&events), [reasoning, looking]);
    }

    /// Nothing waits for the stop reason: each block ends as soon as it
    /// stops, in the order they came, as dsh's adapters send them.
    #[test]
    fn each_block_ends_as_it_stops() {
        let mut decoder = Decoder::anthropic(MODEL);
        let mut feed = |data: &str| decoder.feed(data).expect("the event decodes");

        feed(START);
        feed(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"clock","input":{}}}"#,
        );
        assert_eq!(
            feed(BLOCK_STOP),
            [Event::BlockEnd {
                index: 0,
                block: Block::ToolCall(ToolCall {
                    id: "toolu_1".into(),
                    name: "clock".into(),
                    arguments: "{}".into(),
                })
            }]
        );
        feed(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        );
        assert_eq!(
            feed(
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Done."}}"#
            ),
            [text_delta(1, "Done.")]
        );
        assert_eq!(
            feed(r#"{"type":"content_block_stop","index":1}"#),
            [Event::BlockEnd {
                index: 1,
                block: Block::Text("Done.".into())
            }]
        );
        assert_eq!(feed(&stop("tool_use", "null")), []);
        let events = feed(MESSAGE_STOP);
        assert!(matches!(events[0], Event::Usage { .. }));
        assert_eq!(finish(&events), Some(FinishReason::ToolCalls));
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

    /// The replay a signature from DeepSeek's `DS_MODEL` gets.
    fn ds_signed(signature: &str) -> Option<Replay> {
        Some(Replay::new(
            Origin::new("deepseek-messages", "deepseek", DS_MODEL),
            signature,
        ))
    }

    fn deepseek_error(events: &[&str]) -> ModelError {
        deepseek_decode(events).expect_err("the call fails")
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

        // Blocks are numbered in the order they start, whatever index the
        // wire gives them (`translate.ts:121`).
        assert_eq!(
            events,
            [
                start(0, BlockType::Reasoning),
                // The text the block started with comes first, as a delta.
                Event::ReasoningDelta {
                    index: 0,
                    text: "first".into()
                },
                Event::ReasoningDelta {
                    index: 0,
                    text: " thought".into()
                },
                Event::BlockEnd {
                    index: 0,
                    block: Block::Reasoning {
                        text: "first thought".into(),
                        replay: ds_signed("signature"),
                    }
                },
                start(1, BlockType::ToolCall),
                // dsh names the call once, in an empty delta at its start,
                // and its pieces after that carry only the id.
                Event::ToolCallDelta {
                    index: 1,
                    id: "call_1".into(),
                    name: Some("read".into()),
                    arguments_delta: String::new()
                },
                Event::ToolCallDelta {
                    index: 1,
                    id: "call_1".into(),
                    name: None,
                    arguments_delta: r#"{"path":"#.into()
                },
                Event::ToolCallDelta {
                    index: 1,
                    id: "call_1".into(),
                    name: None,
                    arguments_delta: r#" "a"}"#.into()
                },
                Event::BlockEnd {
                    index: 1,
                    block: Block::ToolCall(ToolCall {
                        id: "call_1".into(),
                        name: "read".into(),
                        arguments: r#"{"path": "a"}"#.into(),
                    })
                },
                Event::Usage {
                    usage: Usage {
                        input_tokens: 12,
                        output_tokens: 5,
                        cache_read_tokens: 30,
                        cache_write_tokens: 7,
                    }
                },
                Event::Finish {
                    reason: FinishReason::ToolCalls
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
        assert_eq!(
            events[..3],
            [
                start(0, BlockType::Text),
                text_delta(0, "initial"),
                Event::BlockEnd {
                    index: 0,
                    block: Block::Text("initial".into())
                },
            ]
        );
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
    /// all keeps no replay. The cut-off tool streams, and the assembler
    /// drops it.
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
        assert_eq!(blocks(&events).len(), 3);
        assert_eq!(
            assembled(&events),
            [
                Block::Reasoning {
                    text: String::new(),
                    replay: ds_signed("opaque"),
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
            assert_eq!(finish(&events), Some(FinishReason::Stop));
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
        assert_eq!(events[1], text_delta(0, "Hel"));
    }
}
