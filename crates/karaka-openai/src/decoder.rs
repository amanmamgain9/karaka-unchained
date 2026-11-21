//! Streamed Chat Completions chunks, turned into [`Event`]s.
//!
//! The [`Decoder`] only sees the JSON of each chunk, one at a time. Reading
//! those out of the HTTP body is a separate job, so everything here can be
//! tested without a network.

use std::collections::BTreeMap;

use karaka_model::{
    Block, ErrorKind, Event, ModelError, StopReason, ToolCall, Usage, is_context_overflow,
    is_out_of_quota,
};
use serde::Deserialize;
use serde_json::Value;

/// What the server sends after the last chunk. It isn't JSON.
const DONE: &str = "[DONE]";

/// Assembles one streamed response.
///
/// This format has no block boundaries: text, reasoning and tool calls all
/// arrive as pieces of one `delta` per chunk, and nothing says when one of
/// them is finished. So the decoder keeps all of them until the end and
/// only then hands out the finished blocks.
#[derive(Default)]
pub(crate) struct Decoder {
    reasoning: String,
    text: String,
    /// Tool calls by the server's `index`. Parallel calls can arrive
    /// interleaved, and a `BTreeMap` gives them back in index order.
    tool_calls: BTreeMap<u32, PartialCall>,
    stop: Option<StopReason>,
    usage: Usage,
}

/// A tool call whose pieces are still arriving.
#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl Decoder {
    /// Reads one chunk's JSON, or the final `[DONE]`.
    ///
    /// Returns a list because one chunk can mean several events: a chunk
    /// can carry both reasoning and text, and `[DONE]` releases every
    /// finished block and then [`Event::Done`] at once. Most chunks give one
    /// event or none, and an empty `Vec` costs no allocation.
    ///
    /// An error payload, a finish reason that isn't a finished answer, or
    /// JSON that doesn't fit ends the call with an `Err`.
    pub(crate) fn feed(&mut self, data: &str) -> Result<Vec<Event>, ModelError> {
        if data == DONE {
            // pi-ai fails a stream that ends without a finish reason
            // (`api/openai-completions.js:484-486`), and dsh counts that
            // as a connection failure (`llm-pi-ai/src/stream.ts:58`), as
            // `end_of_body` does.
            let stop = self.stop.take().ok_or_else(|| {
                ModelError::new(
                    ErrorKind::Network,
                    "the server finished without saying why the model stopped",
                )
            })?;
            return self.finish(stop);
        }

        // A chunk that can't be read is malformed, as llm-deepseek calls it
        // (`sse.ts:18-19`).
        let chunk: WireChunk = serde_json::from_str(data).map_err(|error| {
            ModelError::new(
                ErrorKind::Malformed,
                format!("the server sent a chunk that couldn't be read: {error}"),
            )
        })?;
        if let Some(error) = chunk.error {
            return Err(stream_error(error));
        }

        let mut events = Vec::new();
        for choice in chunk.choices {
            if let Some(delta) = choice.delta {
                self.delta(delta, &mut events);
            }
            // Some servers send `""` rather than `null` until the end.
            if let Some(reason) = choice.finish_reason.filter(|reason| !reason.is_empty()) {
                self.stop = Some(stop_reason(&reason)?);
            }
        }
        // Usage usually comes in a last chunk of its own, with no choices.
        // A server that sends it more than once sends running totals, so the
        // latest replaces the earlier ones.
        if let Some(usage) = chunk.usage {
            self.usage = usage.into_usage();
        }
        Ok(events)
    }

    /// Called when the body ends without `[DONE]`.
    ///
    /// Some local servers end the body without it. If the model had already
    /// said why it stopped, the answer is whole, so it finishes as normal.
    /// Otherwise the stream broke off.
    pub(crate) fn end_of_body(&mut self) -> Result<Vec<Event>, ModelError> {
        let stop = self.stop.take().ok_or_else(|| {
            ModelError::new(
                ErrorKind::Network,
                "the stream ended before the model finished",
            )
        })?;
        self.finish(stop)
    }

    fn delta(&mut self, delta: Delta, events: &mut Vec<Event>) {
        // DeepSeek calls it `reasoning_content`, some other servers
        // `reasoning`. Only one is used, in case a server sends both with
        // the same text. DeepSeek's first chunk carries an empty string,
        // which is no reasoning at all.
        let reasoning = [delta.reasoning_content, delta.reasoning]
            .into_iter()
            .flatten()
            .find(|text| !text.is_empty());
        if let Some(more) = reasoning {
            self.reasoning.push_str(&more);
            events.push(Event::ReasoningDelta(more));
        }

        if let Some(more) = delta.content.filter(|text| !text.is_empty()) {
            self.text.push_str(&more);
            events.push(Event::TextDelta(more));
        }

        // Tool arguments arrive as pieces of JSON that only parse once
        // they're all there, so they aren't passed on one by one.
        for (position, piece) in delta.tool_calls.into_iter().enumerate() {
            // A server that sends each call whole may leave out `index`;
            // then the call's place in the list stands in for it.
            let index = piece.index.unwrap_or(position as u32);
            let call = self.tool_calls.entry(index).or_default();
            // The id and the name usually come only in the first piece. A
            // server that repeats them sends them whole each time, so they
            // replace rather than add up.
            if let Some(id) = piece.id.filter(|id| !id.is_empty()) {
                call.id = id;
            }
            if let Some(function) = piece.function {
                if let Some(name) = function.name.filter(|name| !name.is_empty()) {
                    call.name = name;
                }
                if let Some(arguments) = function.arguments {
                    call.arguments.push_str(&arguments);
                }
            }
        }
    }

    /// The finished blocks, then [`Event::Done`]. Reasoning comes before the
    /// text it led to, and tool calls come last, in index order.
    ///
    /// Fails with [`ErrorKind::EmptyResponse`] on a normal stop with no
    /// blocks at all.
    fn finish(&mut self, stop: StopReason) -> Result<Vec<Event>, ModelError> {
        let mut events = Vec::new();
        let reasoning = std::mem::take(&mut self.reasoning);
        if !reasoning.is_empty() {
            // There's no signature to keep: this format has none. DeepSeek
            // wants the text itself back, which the block already holds.
            events.push(Event::Block(Block::Reasoning {
                text: reasoning,
                replay: None,
            }));
        }
        let text = std::mem::take(&mut self.text);
        if !text.is_empty() {
            events.push(Event::Block(Block::Text(text)));
        }
        let mut tool_calls = std::mem::take(&mut self.tool_calls);
        // The limit can cut a call's arguments off partway, so a call from a
        // `length` finish never goes out, as dsh drops it
        // (`llm/src/assembler.ts:137-141`). Everything arrives only now, so
        // nothing has to be held back to do this.
        if stop == StopReason::MaxTokens {
            tool_calls.clear();
        }
        for (index, call) in tool_calls {
            events.push(Event::Block(Block::ToolCall(ToolCall {
                // The result has to name the call it answers, so a call the
                // server gave no id gets one that is unique in this answer.
                id: if call.id.is_empty() {
                    format!("call_{index}")
                } else {
                    call.id
                },
                name: call.name,
                // A tool without arguments may send no JSON at all.
                arguments: if call.arguments.is_empty() {
                    "{}".into()
                } else {
                    call.arguments
                },
            })));
        }
        // A finished answer with nothing in it would end the turn with
        // nothing to show or act on, so dsh retries it instead
        // (`llm-pi-ai/src/stream.ts:96-107`). Any block counts, so an
        // answer that is only reasoning is kept.
        if stop == StopReason::EndTurn && events.is_empty() {
            return Err(ModelError::new(
                ErrorKind::EmptyResponse,
                "the server finished an answer with no content",
            ));
        }
        events.push(Event::Done {
            stop,
            usage: self.usage,
        });
        Ok(events)
    }
}

/// What a finish reason means, mapped as pi-ai maps it
/// (`api/openai-completions.js:1192-1214`). Only some reasons are an answer.
/// The others fail the call.
fn stop_reason(reason: &str) -> Result<StopReason, ModelError> {
    match reason {
        // `end` is what some servers send for `stop`.
        "stop" | "end" => Ok(StopReason::EndTurn),
        // `function_call` is the name from before parallel tool calls.
        "tool_calls" | "function_call" => Ok(StopReason::ToolCalls),
        "length" => Ok(StopReason::MaxTokens),
        "content_filter" => Err(ModelError::new(
            ErrorKind::Refused,
            "the server's content filter stopped the answer",
        )),
        // pi-ai fails the call, and dsh files its message as a connection
        // failure (`llm-pi-ai/src/stream.ts:59`).
        "network_error" => Err(ModelError::new(
            ErrorKind::Network,
            "the server lost its connection to the model",
        )),
        // DeepSeek's documented reason for running out of capacity partway.
        // pi-ai would fail it as unknown, which dsh doesn't retry; Karaka
        // calls it what it is, a failure on the server's side.
        "insufficient_system_resource" => Err(ModelError::new(
            ErrorKind::Unavailable,
            "the server ran out of capacity before the model finished",
        )),
        // z.ai's `model_context_window_exceeded`, which pi-ai's overflow
        // patterns know (`utils/overflow.js:55`).
        _ if is_context_overflow(reason) => Err(ModelError::new(
            ErrorKind::ContextWindowExceeded,
            format!("the server stopped with `{reason}`"),
        )),
        // Unknown reasons used to keep their name, as a normal stop, so a
        // new one wouldn't break anything. But then a new kind of failure
        // would pass for an answer. pi-ai fails the call instead, and so
        // does Karaka now.
        _ => Err(ModelError::new(
            ErrorKind::Malformed,
            format!("the server stopped for a reason Karaka doesn't know: {reason}"),
        )),
    }
}

/// An error sent in place of a chunk.
fn stream_error(error: WireError) -> ModelError {
    let kind = classify(&error.detail(), None);
    let message = error
        .message
        .unwrap_or_else(|| "the server sent an error with no message".into());
    ModelError::new(kind, message)
}

/// The kind of failure behind an error: from what the server said, and the
/// HTTP status when it came as a failed response (`None` when it came in
/// the stream).
///
/// Servers differ in their error types, so the status decides. The text is
/// read only for what no status says: an account out of credit, which can
/// come back as 429 but isn't a rate limit, and a conversation too long for
/// the model, which comes back as a plain 400. The checks run in
/// llm-deepseek's order (`transport.ts:28-35`).
pub(crate) fn classify(detail: &str, status: Option<u16>) -> ErrorKind {
    if matches!(status, Some(401 | 403)) {
        ErrorKind::Auth
    } else if status == Some(402) || is_out_of_quota(detail) {
        // 402 is DeepSeek's "Insufficient Balance".
        ErrorKind::Quota
    } else if status == Some(429) {
        ErrorKind::RateLimited
    } else if is_context_overflow(detail) {
        ErrorKind::ContextWindowExceeded
    } else if status.is_none_or(|status| status >= 500) {
        // A 5xx, or an error in the stream after the server had accepted
        // the request.
        ErrorKind::Unavailable
    } else {
        // 400, 404, 413, 422 and every other status. dsh calls the ones it
        // doesn't name `HTTP_<status>`, and doesn't retry them.
        ErrorKind::InvalidRequest
    }
}

// What the server sends. Only the fields Karaka reads are declared; serde
// skips the rest, so fields added by OpenAI or by another server are
// ignored rather than fatal. Almost everything is optional because servers
// differ in what they leave out, and many send `null` for "nothing".

#[derive(Deserialize)]
struct WireChunk {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<WireUsage>,
    error: Option<WireError>,
}

#[derive(Deserialize)]
struct Choice {
    delta: Option<Delta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Delta {
    content: Option<String>,
    reasoning_content: Option<String>,
    reasoning: Option<String>,
    /// `default` covers a missing field, and `null_as_empty` an explicit
    /// `null`, which some servers send on chunks with no tool calls.
    #[serde(default, deserialize_with = "null_as_empty")]
    tool_calls: Vec<ToolCallPiece>,
}

#[derive(Deserialize)]
struct ToolCallPiece {
    index: Option<u32>,
    id: Option<String>,
    function: Option<FunctionPiece>,
}

#[derive(Deserialize)]
struct FunctionPiece {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    /// OpenAI's count of input read from its cache.
    prompt_tokens_details: Option<PromptDetails>,
    /// DeepSeek's name for the same count.
    prompt_cache_hit_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct PromptDetails {
    cached_tokens: Option<u64>,
}

impl WireUsage {
    /// Karaka's counts don't overlap, but `prompt_tokens` includes the
    /// cached input, so the cached part is taken out of it. Saturating, so a
    /// server that miscounts can't make it wrap.
    fn into_usage(self) -> Usage {
        let cached = self
            .prompt_tokens_details
            .and_then(|details| details.cached_tokens)
            .or(self.prompt_cache_hit_tokens)
            .unwrap_or(0);
        Usage {
            input_tokens: self.prompt_tokens.saturating_sub(cached),
            output_tokens: self.completion_tokens,
            cache_read_tokens: cached,
            // Caching here is automatic, and no server reports writes.
            cache_write_tokens: 0,
        }
    }
}

/// Also the `error` field of a failed HTTP response's body.
#[derive(Deserialize)]
pub(crate) struct WireError {
    pub(crate) message: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    /// Usually a string, but some servers send a number, so it isn't
    /// declared as one.
    code: Option<Value>,
}

impl WireError {
    /// The code, type and message joined, which is all the text that
    /// [`classify`] reads, as llm-deepseek joins them (`transport.ts:27`).
    pub(crate) fn detail(&self) -> String {
        let code = self.code.as_ref().and_then(Value::as_str);
        [code, self.kind.as_deref(), self.message.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Reads a list that a server may send as `null`.
fn null_as_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use karaka_model::{Block, ErrorKind, Event, ModelError, StopReason, ToolCall, Usage};

    use super::{Decoder, classify};

    /// Feeds every chunk and keeps what comes out.
    fn decode(chunks: &[&str]) -> Result<Vec<Event>, ModelError> {
        let mut decoder = Decoder::default();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(decoder.feed(chunk)?);
        }
        Ok(out)
    }

    /// Feeds chunks until one fails, and gives back what came out before
    /// the failure, and the failure. As a stream does, a caller sees both.
    fn decode_until_error(chunks: &[&str]) -> (Vec<Event>, ModelError) {
        let mut decoder = Decoder::default();
        let mut out = Vec::new();
        for chunk in chunks {
            match decoder.feed(chunk) {
                Ok(more) => out.extend(more),
                Err(error) => return (out, error),
            }
        }
        panic!("expected a failure, got {out:?}");
    }

    /// A chunk that finishes with `reason` and carries nothing else.
    fn finish(reason: &str) -> String {
        format!(r#"{{"choices":[{{"index":0,"delta":{{}},"finish_reason":"{reason}"}}]}}"#)
    }

    const HI: &str = r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#;

    /// The `Done` at the end of `events`, which must be there.
    fn done(events: &[Event]) -> (&StopReason, &Usage) {
        match events.last() {
            Some(Event::Done { stop, usage }) => (stop, usage),
            other => panic!("expected Done last, got {other:?}"),
        }
    }

    #[test]
    fn a_text_answer() {
        let events = decode(&[
            r#"{"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"Hel"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":5,"total_tokens":25}}"#,
            "[DONE]",
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
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                    },
                },
            ]
        );
    }

    #[test]
    fn deepseek_reasoning_then_the_answer() {
        let events = decode(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":""},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":null,"reasoning_content":"think"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":null,"reasoning_content":"ing"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"answer","reasoning_content":null},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":""},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ])
        .expect("the stream decodes");

        assert_eq!(
            events[..5],
            [
                Event::ReasoningDelta("think".into()),
                Event::ReasoningDelta("ing".into()),
                Event::TextDelta("answer".into()),
                Event::Block(Block::Reasoning {
                    text: "thinking".into(),
                    replay: None,
                }),
                Event::Block(Block::Text("answer".into())),
            ]
        );
        assert_eq!(done(&events).0, &StopReason::EndTurn);
    }

    #[test]
    fn reasoning_under_its_other_name() {
        let events = decode(&[
            r#"{"choices":[{"index":0,"delta":{"reasoning":"hmm"},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ])
        .expect("the stream decodes");

        assert_eq!(events[0], Event::ReasoningDelta("hmm".into()));
    }

    #[test]
    fn two_parallel_tool_calls_whose_pieces_interleave() {
        let events = decode(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":null,"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"clock","arguments":""}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"weather","arguments":"{\"ci"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"zone\": "}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"ty\": \"Oslo\"}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"UTC\"}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":2,"id":"call_c","function":{"name":"ping"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ])
        .expect("the stream decodes");

        let call = |id: &str, name: &str, arguments: &str| {
            Event::Block(Block::ToolCall(ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: arguments.into(),
            }))
        };
        // No deltas: the calls come out whole, in index order.
        assert_eq!(
            events[..3],
            [
                call("call_a", "clock", r#"{"zone": "UTC"}"#),
                call("call_b", "weather", r#"{"city": "Oslo"}"#),
                // A call with no arguments gets an empty object.
                call("call_c", "ping", "{}"),
            ]
        );
        assert_eq!(done(&events).0, &StopReason::ToolCalls);
    }

    #[test]
    fn deepseek_cache_hits_are_taken_out_of_the_input_count() {
        let events = decode(&[
            r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}],"usage":null}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":7,"total_tokens":127,"prompt_cache_hit_tokens":100,"prompt_cache_miss_tokens":20,"completion_tokens_details":{"reasoning_tokens":0}}}"#,
            "[DONE]",
        ])
        .expect("the stream decodes");

        assert_eq!(
            done(&events).1,
            &Usage {
                input_tokens: 20,
                output_tokens: 7,
                cache_read_tokens: 100,
                cache_write_tokens: 0,
            }
        );
    }

    #[test]
    fn openai_cache_hits_are_taken_out_of_the_input_count() {
        let events = decode(&[
            r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":40,"audio_tokens":0}}}"#,
            "[DONE]",
        ])
        .expect("the stream decodes");

        let usage = done(&events).1;
        assert_eq!((usage.input_tokens, usage.cache_read_tokens), (10, 40));
    }

    #[test]
    fn a_body_that_ends_after_the_finish_reason_still_finishes() {
        let mut decoder = Decoder::default();
        decoder
            .feed(r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"length"}]}"#)
            .expect("the chunk decodes");

        let events = decoder.end_of_body().expect("the answer is whole");
        assert_eq!(events[0], Event::Block(Block::Text("hi".into())));
        assert_eq!(done(&events).0, &StopReason::MaxTokens);
    }

    #[test]
    fn a_body_that_ends_before_the_finish_reason_broke_off() {
        let mut decoder = Decoder::default();
        decoder
            .feed(r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#)
            .expect("the chunk decodes");

        let error = decoder.end_of_body().expect_err("the answer is cut off");
        assert_eq!(error.kind, ErrorKind::Network);
    }

    /// pi-ai fails a stream that ends with no finish reason, and dsh files
    /// that as a connection failure. Its test "maps pi-ai transport wording"
    /// (`llm-pi-ai/tests/convert.spec.ts`) has a case for it.
    #[test]
    fn done_without_a_finish_reason_is_an_error() {
        let error = decode(&[HI, "[DONE]"]).expect_err("the call fails");
        assert_eq!(error.kind, ErrorKind::Network);
    }

    #[test]
    fn a_mid_stream_error_ends_the_call() {
        let error = decode(&[
            r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
            r#"{"error":{"message":"The server had an error","type":"server_error","code":null}}"#,
        ])
        .expect_err("the call fails");

        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert_eq!(error.message, "The server had an error");
    }

    #[test]
    fn an_unknown_finish_reason_is_malformed() {
        let (_, error) = decode_until_error(&[HI, &finish("mystery"), "[DONE]"]);
        assert_eq!(error.kind, ErrorKind::Malformed);
        assert!(!error.kind.is_retryable());
        assert!(error.message.contains("mystery"), "{error}");
    }

    #[test]
    fn deepseek_running_out_of_capacity_is_unavailable() {
        let (_, error) =
            decode_until_error(&[HI, &finish("insufficient_system_resource"), "[DONE]"]);
        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert!(error.kind.is_retryable());
    }

    /// The text that streamed before the filter stopped the answer has gone
    /// out, but the call ends in an `Err`, so nothing takes it for the
    /// answer.
    #[test]
    fn a_content_filter_is_a_refusal() {
        let (events, error) = decode_until_error(&[HI, &finish("content_filter"), "[DONE]"]);
        assert_eq!(events, [Event::TextDelta("hi".into())]);
        assert_eq!(error.kind, ErrorKind::Refused);
        assert!(!error.kind.is_retryable());
    }

    #[test]
    fn a_full_context_window_as_a_finish_reason_is_an_overflow() {
        let (_, error) =
            decode_until_error(&[HI, &finish("model_context_window_exceeded"), "[DONE]"]);
        assert_eq!(error.kind, ErrorKind::ContextWindowExceeded);
    }

    #[test]
    fn end_is_a_normal_stop() {
        let events = decode(&[HI, &finish("end"), "[DONE]"]).expect("the stream decodes");
        assert_eq!(done(&events).0, &StopReason::EndTurn);
    }

    /// dsh's "classifies a completed stop with no content as an
    /// EMPTY_RESPONSE error" (`llm-pi-ai/tests/convert.spec.ts`).
    #[test]
    fn a_completed_stop_with_no_content_is_an_empty_response() {
        let (_, error) = decode_until_error(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
            &finish("stop"),
            "[DONE]",
        ]);
        assert_eq!(error.kind, ErrorKind::EmptyResponse);
        assert!(error.kind.is_retryable());
    }

    /// dsh's "keeps a thinking-only stop successful (any block counts as
    /// content)" (`llm-pi-ai/tests/convert.spec.ts`).
    #[test]
    fn a_thinking_only_stop_stays_successful() {
        let events = decode(&[
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"mull"},"finish_reason":null}]}"#,
            &finish("stop"),
            "[DONE]",
        ])
        .expect("the stream decodes");
        assert_eq!(
            events[1],
            Event::Block(Block::Reasoning {
                text: "mull".into(),
                replay: None,
            })
        );
        assert_eq!(done(&events).0, &StopReason::EndTurn);
    }

    /// dsh's assembler drops "the tool calls a max-tokens finish drops"
    /// (`llm/tests/assembler.spec.ts`), and keeps what came before them.
    #[test]
    fn a_max_tokens_finish_drops_a_cut_off_tool_call() {
        let events = decode(&[
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"search"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"Looking."},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"search","arguments":"{\"query\": \"wea"}}]},"finish_reason":null}]}"#,
            &finish("length"),
            "[DONE]",
        ])
        .expect("the stream decodes");

        assert_eq!(
            events[2..4],
            [
                Event::Block(Block::Reasoning {
                    text: "search".into(),
                    replay: None,
                }),
                Event::Block(Block::Text("Looking.".into())),
            ]
        );
        assert_eq!(events.len(), 5, "{events:?}");
        assert_eq!(done(&events).0, &StopReason::MaxTokens);
    }

    #[test]
    fn a_mid_stream_quota_error_is_out_of_quota() {
        let (_, error) = decode_until_error(&[
            HI,
            r#"{"error":{"message":"You exceeded your current quota.","type":"insufficient_quota","code":"insufficient_quota"}}"#,
        ]);
        assert_eq!(error.kind, ErrorKind::Quota);
    }

    /// dsh's "recognizes context-capacity details independently of the HTTP
    /// status" (`llm-deepseek/tests/runtime.spec.ts`), and the statuses in
    /// its "classifies status ... and error ..." (`stream.spec.ts`).
    #[test]
    fn only_context_capacity_details_are_an_overflow() {
        assert_eq!(
            classify("request too large for model context", Some(400)),
            ErrorKind::ContextWindowExceeded
        );
        assert_eq!(
            classify("context_length_exceeded", Some(413)),
            ErrorKind::ContextWindowExceeded
        );
        assert_eq!(
            classify("maximum context length exceeded", Some(400)),
            ErrorKind::ContextWindowExceeded
        );
        assert_eq!(
            classify(
                "invalid input: temperature exceeds maximum allowed value",
                Some(400)
            ),
            ErrorKind::InvalidRequest
        );
    }

    #[test]
    fn statuses_map_as_in_dsh() {
        let cases = [
            (401, ErrorKind::Auth),
            (403, ErrorKind::Auth),
            (402, ErrorKind::Quota),
            (429, ErrorKind::RateLimited),
            (400, ErrorKind::InvalidRequest),
            (413, ErrorKind::InvalidRequest),
            (422, ErrorKind::InvalidRequest),
            // dsh's `HTTP_404` and `HTTP_418`, which it doesn't retry.
            (404, ErrorKind::InvalidRequest),
            (418, ErrorKind::InvalidRequest),
            (500, ErrorKind::Unavailable),
            (503, ErrorKind::Unavailable),
        ];
        for (status, expected) in cases {
            assert_eq!(classify("", Some(status)), expected, "{status}");
        }
        assert_eq!(
            classify("insufficient balance", Some(400)),
            ErrorKind::Quota
        );
        assert_eq!(classify("", None), ErrorKind::Unavailable);
    }

    #[test]
    fn unknown_fields_and_delta_keys_are_ignored() {
        let events = decode(&[
            r#"{"id":"c1","system_fingerprint":"fp","service_tier":"default","choices":[{"index":0,"logprobs":null,"delta":{"role":"assistant","refusal":null,"audio":{"id":"a"},"content":"ok","tool_calls":null},"finish_reason":null,"native_finish_reason":null}],"usage":null,"x_extra":[1,2]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ])
        .expect("the stream decodes");

        assert_eq!(
            events[..2],
            [
                Event::TextDelta("ok".into()),
                Event::Block(Block::Text("ok".into())),
            ]
        );
    }

    #[test]
    fn json_that_doesnt_fit_is_an_error() {
        let error = decode(&[r#"{"choices": "nope"}"#]).expect_err("the call fails");
        assert_eq!(error.kind, ErrorKind::Malformed);
        assert!(error.message.contains("couldn't be read"), "{error}");
    }
}
