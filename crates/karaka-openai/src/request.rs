//! A [`Request`] as the JSON body of `POST /chat/completions`.
//!
//! The wire types borrow from the request (`&'a str`) where they can: the
//! body exists only long enough to be written out. Text that has to be
//! joined or changed on the way out, such as several text blocks becoming
//! one string, is the exception and is owned.
//!
//! The format differs from Karaka's in three ways that shape this module:
//!
//! - The system prompt is a message, the first one.
//! - Content is one string per message, not a list of blocks.
//! - Tool results are messages of their own, with the role `tool`.

use karaka_model::{Block, ErrorKind, Message, ModelError, Request, Role, ToolCall, ToolResult};
use serde::Serialize;
use serde_json::value::RawValue;

/// The body of `POST /chat/completions`.
#[derive(Serialize)]
pub(crate) struct Body<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    /// Sent only when the request sets it. Providers' limits and defaults
    /// differ, and a number that suits one model can be refused by another,
    /// so the server's own default is the safe choice.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    /// Without it a streamed answer reports no token counts at all.
    include_usage: bool,
}

/// One message. `tag = "role"` writes the variant's name into the object as
/// `"role": "user"` and so on, which is how the format tells messages apart.
#[derive(Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum WireMessage<'a> {
    System {
        content: &'a str,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        /// The model's reasoning, which DeepSeek's thinking mode needs back.
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<WireToolCall<'a>>,
    },
    Tool {
        tool_call_id: &'a str,
        content: String,
    },
}

#[derive(Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    /// Always `"function"`, the only kind of tool the format has.
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunctionCall<'a>,
}

#[derive(Serialize)]
struct WireFunctionCall<'a> {
    name: &'a str,
    /// A JSON string holding the arguments, not a JSON object. The model's
    /// own text goes back exactly as it wrote it, broken or not.
    arguments: &'a str,
}

#[derive(Serialize)]
struct WireTool<'a> {
    /// Always `"function"`.
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction<'a>,
}

#[derive(Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a RawValue,
}

/// Builds the streaming body for `request`.
///
/// Fails only if a tool's parameters aren't a JSON object, which is a bug in
/// whoever defined the tool, so it's reported before anything is sent.
pub(crate) fn body(request: &Request) -> Result<Body<'_>, ModelError> {
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let parameters = json_object(&tool.parameters).ok_or_else(|| {
                ModelError::new(
                    ErrorKind::InvalidRequest,
                    format!(
                        "tool \"{}\" has parameters that aren't a JSON object",
                        tool.name
                    ),
                )
            })?;
            Ok(WireTool {
                kind: "function",
                function: WireFunction {
                    name: &tool.name,
                    description: &tool.description,
                    parameters,
                },
            })
        })
        .collect::<Result<_, ModelError>>()?;

    let mut messages = Vec::new();
    if let Some(system) = &request.system {
        messages.push(WireMessage::System { content: system });
    }
    for message in &request.messages {
        match message.role {
            Role::User => user(message, &mut messages),
            Role::Assistant => messages.push(assistant(message)),
        }
    }

    Ok(Body {
        model: &request.model,
        messages,
        tools,
        max_tokens: request.max_tokens,
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
    })
}

/// One user message, which can become several wire messages: one `tool`
/// message per tool result, then a `user` message for the text.
///
/// The tool messages come first because they must directly follow the
/// assistant message that made the calls. The `user` message is left out
/// when there is no text beside the results, but kept when there were no
/// results either, so an empty message never silently disappears.
fn user<'a>(message: &'a Message, out: &mut Vec<WireMessage<'a>>) {
    let mut has_results = false;
    for block in &message.content {
        if let Block::ToolResult(result) = block {
            has_results = true;
            out.push(tool_message(result));
        }
    }
    let text = message.text();
    if !text.is_empty() || !has_results {
        out.push(WireMessage::User { content: text });
    }
}

/// A tool result. The format has no `is_error`, so a failure is said in the
/// content itself, where the model reads it.
fn tool_message(result: &ToolResult) -> WireMessage<'_> {
    // DeepSeek's adapter sends this for empty output: some servers refuse a
    // tool message with no content.
    let output = if result.content.is_empty() {
        "(no output)"
    } else {
        &result.content
    };
    let content = if result.is_error {
        format!("Error: {output}")
    } else {
        output.to_owned()
    };
    WireMessage::Tool {
        tool_call_id: &result.call_id,
        content,
    }
}

/// An assistant message: its text, its reasoning and its tool calls.
///
/// Reasoning goes back as `reasoning_content` whenever the message has any.
/// DeepSeek's thinking mode requires it on a message with tool calls and
/// ignores it elsewhere; DeepSeek's own adapter sends it every time, because
/// gateways that forward to other vendors use it to find that turn's
/// thinking again. A message with no reasoning gets no field.
///
/// `content` is `""` when there's no text, never `null`, even beside tool
/// calls. DeepSeek's adapter found that some gateways refuse `null`, and
/// DeepSeek refuses it on a message that has no tool calls either.
fn assistant(message: &Message) -> WireMessage<'_> {
    let reasoning: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Reasoning { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let tool_calls = message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::ToolCall(call) => Some(tool_call(call)),
            _ => None,
        })
        .collect();
    WireMessage::Assistant {
        content: message.text(),
        reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
        tool_calls,
    }
}

fn tool_call(call: &ToolCall) -> WireToolCall<'_> {
    WireToolCall {
        id: &call.id,
        kind: "function",
        function: WireFunctionCall {
            name: &call.name,
            arguments: &call.arguments,
        },
    }
}

/// `text` as raw JSON, if it is a JSON object. Checked, but not parsed into
/// values: it goes out exactly as it came in.
fn json_object(text: &str) -> Option<&RawValue> {
    let raw: &RawValue = serde_json::from_str(text).ok()?;
    raw.get().starts_with('{').then_some(raw)
}

#[cfg(test)]
mod tests {
    use karaka_model::{
        Block, ErrorKind, Message, Request, Role, ToolCall, ToolResult, ToolSchema,
    };
    use serde_json::{Value, json};

    use super::body;

    fn to_json(request: &Request) -> Value {
        serde_json::to_value(body(request).expect("the body builds")).expect("it serializes")
    }

    fn result(call_id: &str, content: &str, is_error: bool) -> Block {
        Block::ToolResult(ToolResult {
            call_id: call_id.into(),
            content: content.into(),
            is_error,
        })
    }

    #[test]
    fn a_plain_question() {
        let mut request = Request::new("gpt-test", vec![Message::user("hi")]);
        request.system = Some("Be brief.".into());

        assert_eq!(
            to_json(&request),
            json!({
                "model": "gpt-test",
                "messages": [
                    {"role": "system", "content": "Be brief."},
                    {"role": "user", "content": "hi"}
                ],
                "stream": true,
                "stream_options": {"include_usage": true}
            })
        );
    }

    #[test]
    fn a_tool_round_trip() {
        let mut request = Request::new(
            "deepseek-test",
            vec![
                Message::user("what time is it in two places?"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Block::Reasoning {
                            text: "check both clocks".into(),
                            replay: None,
                        },
                        Block::Text("Checking.".into()),
                        Block::ToolCall(ToolCall {
                            id: "call_1".into(),
                            name: "clock".into(),
                            arguments: r#"{"zone": "UTC"}"#.into(),
                        }),
                        Block::ToolCall(ToolCall {
                            id: "call_2".into(),
                            name: "clock".into(),
                            // Broken JSON goes back as the model wrote it.
                            arguments: r#"{"zone": "#.into(),
                        }),
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        Block::Text("Also, ".into()),
                        result("call_1", "12:00", false),
                        result("call_2", "", false),
                        Block::Text("thanks.".into()),
                    ],
                },
            ],
        );
        request.max_tokens = Some(64);
        request.tools.push(ToolSchema {
            name: "clock".into(),
            description: "Tells the time.".into(),
            parameters: r#"{"type": "object"}"#.into(),
        });

        assert_eq!(
            to_json(&request),
            json!({
                "model": "deepseek-test",
                "messages": [
                    {"role": "user", "content": "what time is it in two places?"},
                    {
                        "role": "assistant",
                        "content": "Checking.",
                        "reasoning_content": "check both clocks",
                        "tool_calls": [
                            {"id": "call_1", "type": "function",
                             "function": {"name": "clock", "arguments": "{\"zone\": \"UTC\"}"}},
                            {"id": "call_2", "type": "function",
                             "function": {"name": "clock", "arguments": "{\"zone\": "}}
                        ]
                    },
                    {"role": "tool", "tool_call_id": "call_1", "content": "12:00"},
                    {"role": "tool", "tool_call_id": "call_2", "content": "(no output)"},
                    {"role": "user", "content": "Also, thanks."}
                ],
                "tools": [
                    {"type": "function", "function": {
                        "name": "clock",
                        "description": "Tells the time.",
                        "parameters": {"type": "object"}
                    }}
                ],
                "max_tokens": 64,
                "stream": true,
                "stream_options": {"include_usage": true}
            })
        );
    }

    #[test]
    fn tool_results_alone_make_no_user_message() {
        let request = Request::new(
            "gpt-test",
            vec![Message {
                role: Role::User,
                content: vec![result("call_1", "12:00", false)],
            }],
        );
        assert_eq!(
            to_json(&request)["messages"],
            json!([{"role": "tool", "tool_call_id": "call_1", "content": "12:00"}])
        );
    }

    #[test]
    fn a_failed_tool_result_says_so_in_its_content() {
        let request = Request::new(
            "gpt-test",
            vec![Message {
                role: Role::User,
                content: vec![result("call_1", "no such zone", true)],
            }],
        );
        assert_eq!(
            to_json(&request)["messages"][0]["content"],
            "Error: no such zone"
        );
    }

    #[test]
    fn a_tool_call_without_text_or_reasoning_sends_empty_content() {
        let request = Request::new(
            "gpt-test",
            vec![Message {
                role: Role::Assistant,
                content: vec![Block::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "clock".into(),
                    arguments: "{}".into(),
                })],
            }],
        );
        let message = &to_json(&request)["messages"][0];
        assert_eq!(message["content"], "");
        assert!(message.get("reasoning_content").is_none(), "{message}");
    }

    #[test]
    fn max_tokens_is_left_out_when_unset() {
        let request = Request::new("gpt-test", vec![Message::user("hi")]);
        assert!(to_json(&request).get("max_tokens").is_none());
    }

    #[test]
    fn a_tool_with_broken_parameters_is_rejected_before_sending() {
        let mut request = Request::new("gpt-test", vec![Message::user("hi")]);
        request.tools.push(ToolSchema {
            name: "clock".into(),
            description: "Tells the time.".into(),
            parameters: "[1, 2]".into(),
        });
        let Err(error) = body(&request) else {
            panic!("expected the body to fail");
        };
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert!(error.message.contains("\"clock\""), "{error}");
    }
}
