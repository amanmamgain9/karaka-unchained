//! Streamed Chat Completions chunks, turned into [`Event`]s.
//!
//! The [`Decoder`] only sees the JSON of each chunk, one at a time. Reading
//! those out of the HTTP body is a separate job, so everything here can be
//! tested without a network.

use std::collections::BTreeMap;

use karaka_model::{Block, ErrorKind, Event, ModelError, StopReason, ToolCall, Usage};
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
    /// An error payload, or JSON that doesn't fit, ends the call with an
    /// `Err`.
    pub(crate) fn feed(&mut self, data: &str) -> Result<Vec<Event>, ModelError> {
        if data == DONE {
            let stop = self.stop.take().ok_or_else(|| {
                ModelError::new(
                    ErrorKind::Unavailable,
                    "the server finished without saying why the model stopped",
                )
            })?;
            return Ok(self.finish(stop));
        }

        let chunk: WireChunk = serde_json::from_str(data).map_err(|error| {
            ModelError::new(
                ErrorKind::Unavailable,
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
                self.stop = Some(stop_reason(reason));
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
        Ok(self.finish(stop))
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
    fn finish(&mut self, stop: StopReason) -> Vec<Event> {
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
        for (index, call) in std::mem::take(&mut self.tool_calls) {
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
        events.push(Event::Done {
            stop,
            usage: self.usage,
        });
        events
    }
}

fn stop_reason(reason: String) -> StopReason {
    match reason.as_str() {
        "stop" => StopReason::EndTurn,
        // `function_call` is the name from before parallel tool calls.
        "tool_calls" | "function_call" => StopReason::ToolCalls,
        "length" => StopReason::MaxTokens,
        // `content_filter`, DeepSeek's `insufficient_system_resource`, and
        // whatever comes next keep their own name. The loop runs tools only
        // on `ToolCalls`, which matters: a filter can cut a call off halfway.
        _ => StopReason::Other(reason),
    }
}

/// An error sent in place of a chunk. The server had already accepted the
/// request, so the failure is on its side, unless the account has run out
/// of credit, which waiting won't fix.
fn stream_error(error: WireError) -> ModelError {
    let kind = if error.is_out_of_quota() {
        ErrorKind::Auth
    } else {
        ErrorKind::Unavailable
    };
    let message = error
        .message
        .unwrap_or_else(|| "the server sent an error with no message".into());
    ModelError::new(kind, message)
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
    /// Whether the account is out of credit. OpenAI says so in both `type`
    /// and `code`.
    pub(crate) fn is_out_of_quota(&self) -> bool {
        let code = self.code.as_ref().and_then(Value::as_str);
        [self.kind.as_deref(), code].contains(&Some("insufficient_quota"))
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

    use super::Decoder;

    /// Feeds every chunk and keeps what comes out.
    fn decode(chunks: &[&str]) -> Result<Vec<Event>, ModelError> {
        let mut decoder = Decoder::default();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(decoder.feed(chunk)?);
        }
        Ok(out)
    }

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

    #[test]
    fn done_without_a_finish_reason_is_an_error() {
        let error = decode(&[
            r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
            "[DONE]",
        ])
        .expect_err("the call fails");
        assert_eq!(error.kind, ErrorKind::Unavailable);
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
    fn unknown_reasons_keep_their_name() {
        let events = decode(&[
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"insufficient_system_resource"}]}"#,
            "[DONE]",
        ])
        .expect("the stream decodes");

        assert_eq!(
            done(&events).0,
            &StopReason::Other("insufficient_system_resource".into())
        );
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
        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert!(error.message.contains("couldn't be read"), "{error}");
    }
}
