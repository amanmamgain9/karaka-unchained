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
//!
//! Like the Anthropic provider, it leaves out a message whose text is empty
//! or only whitespace and that has nothing else in it. Some servers refuse
//! such messages, and a conversation that came from another provider can
//! hold them.

use karaka_model::{
    Block, Effort, ErrorKind, Message, ModelError, Request, Role, ToolCall, ToolResult,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// The settings that come from the provider's config rather than from each
/// request.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Settings {
    /// For a request that doesn't set `effort`.
    pub default_effort: Option<Effort>,
    /// DeepSeek's switch for thinking. `None` sends no switch.
    pub thinking: Option<bool>,
    pub max_tokens_field: MaxTokensField,
}

/// Which field carries a request's `max_tokens`.
///
/// OpenAI renamed it to `max_completion_tokens`, and its reasoning models
/// refuse the old name. Most other servers, DeepSeek among them, know only
/// the old one. pi-ai has the same switch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    /// `max_tokens`, which nearly every server accepts.
    #[default]
    MaxTokens,
    /// `max_completion_tokens`, for OpenAI's own newer models.
    MaxCompletionTokens,
}

/// The body of `POST /chat/completions`.
#[derive(Serialize)]
pub(crate) struct Body<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    /// Sent only when the request sets it. Providers' limits and defaults
    /// differ, and a number that suits one model can be refused by another,
    /// so the server's own default is the safe choice. At most one of this
    /// and `max_completion_tokens` is sent, as the config chooses.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
    /// Only DeepSeek knows this field, so it is sent only when the config
    /// sets it.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingSwitch>,
    stream: bool,
    stream_options: StreamOptions,
}

/// DeepSeek's `{"type": "enabled"}` or `{"type": "disabled"}`.
#[derive(Serialize)]
struct ThinkingSwitch {
    #[serde(rename = "type")]
    kind: &'static str,
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
/// Fails before anything is sent if a tool's parameters aren't a JSON
/// object, which is a bug in whoever defined the tool, or if the request
/// asks for an effort while the config turns thinking off.
pub(crate) fn body<'a>(request: &'a Request, settings: &Settings) -> Result<Body<'a>, ModelError> {
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

    let effort = request.effort.or(settings.default_effort);
    // DeepSeek has no effort for thinking that is off. dsh refuses this pair
    // before sending, rather than let one setting quietly win.
    if let (Some(effort), Some(false)) = (effort, settings.thinking) {
        return Err(ModelError::new(
            ErrorKind::InvalidRequest,
            format!("the effort is {effort:?}, but the provider's config turns thinking off"),
        ));
    }

    let mut messages = Vec::new();
    if let Some(system) = request
        .system
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        messages.push(WireMessage::System { content: system });
    }
    for message in &request.messages {
        match message.role {
            Role::User => user(message, &mut messages),
            Role::Assistant => messages.extend(assistant(message)),
        }
    }

    let (max_tokens, max_completion_tokens) = match settings.max_tokens_field {
        MaxTokensField::MaxTokens => (request.max_tokens, None),
        MaxTokensField::MaxCompletionTokens => (None, request.max_tokens),
    };
    Ok(Body {
        model: &request.model,
        messages,
        tools,
        max_tokens,
        max_completion_tokens,
        reasoning_effort: effort.map(effort_name),
        thinking: settings.thinking.map(|on| ThinkingSwitch {
            kind: if on { "enabled" } else { "disabled" },
        }),
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
    })
}

/// The name each server family uses for an effort level, as
/// `reasoning_effort`.
///
/// | Effort   | `reasoning_effort` | OpenAI          | DeepSeek        |
/// |----------|--------------------|-----------------|-----------------|
/// | `Low`    | `"low"`            | yes             | some models     |
/// | `Medium` | `"medium"`         | yes             | no              |
/// | `High`   | `"high"`           | yes             | yes, default    |
/// | `XHigh`  | `"xhigh"`          | newer models    | no              |
/// | `Max`    | `"max"`            | newest models   | yes             |
///
/// Each level keeps its own name, with no rounding to a nearby one. This
/// provider can't tell which server or model it is talking to, and every
/// rounding rule is wrong for some of them: `Max` becoming `"xhigh"` would
/// suit older OpenAI models but take `max` away from the newest ones and
/// from DeepSeek. A server that lacks a level refuses it with a 400, which
/// names the problem instead of hiding it.
fn effort_name(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

/// One user message, which can become several wire messages: one `tool`
/// message per tool result, then a `user` message for the text.
///
/// The tool messages come first because they must directly follow the
/// assistant message that made the calls. The `user` message is left out
/// when its text is empty or only whitespace, with or without results:
/// there is nothing in it to send.
fn user<'a>(message: &'a Message, out: &mut Vec<WireMessage<'a>>) {
    for block in &message.content {
        if let Block::ToolResult(result) = block {
            out.push(tool_message(result));
        }
    }
    let text = message.text();
    if !text.trim().is_empty() {
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
/// DeepSeek refuses it on a message that has no tool calls either. Text that
/// is only whitespace counts as none.
///
/// A message left with no text, no reasoning and no tool calls is `None`,
/// and isn't sent.
fn assistant(message: &Message) -> Option<WireMessage<'_>> {
    let reasoning: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Reasoning { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let tool_calls: Vec<_> = message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::ToolCall(call) => Some(tool_call(call)),
            _ => None,
        })
        .collect();
    let mut content = message.text();
    if content.trim().is_empty() {
        content.clear();
    }
    let has_reasoning = !reasoning.trim().is_empty();
    if content.is_empty() && !has_reasoning && tool_calls.is_empty() {
        return None;
    }
    Some(WireMessage::Assistant {
        content,
        reasoning_content: has_reasoning.then_some(reasoning),
        tool_calls,
    })
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
        Block, Effort, ErrorKind, Message, Request, Role, ToolCall, ToolResult, ToolSchema,
    };
    use serde_json::{Value, json};

    use super::{MaxTokensField, Settings, body};

    fn json_with(request: &Request, settings: &Settings) -> Value {
        serde_json::to_value(body(request, settings).expect("the body builds"))
            .expect("it serializes")
    }

    fn to_json(request: &Request) -> Value {
        json_with(request, &Settings::default())
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
        let Err(error) = body(&request, &Settings::default()) else {
            panic!("expected the body to fail");
        };
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert!(error.message.contains("\"clock\""), "{error}");
    }

    // Effort, thinking and the max-tokens field.

    #[test]
    fn every_effort_level_keeps_its_name_as_reasoning_effort() {
        let levels = [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::XHigh, "xhigh"),
            (Effort::Max, "max"),
        ];
        for (effort, name) in levels {
            let mut request = Request::new("gpt-test", vec![Message::user("hi")]);
            request.effort = Some(effort);
            assert_eq!(to_json(&request)["reasoning_effort"], name);
        }
    }

    #[test]
    fn the_configs_effort_applies_when_the_request_sets_none() {
        let settings = Settings {
            default_effort: Some(Effort::High),
            ..Settings::default()
        };
        let mut request = Request::new("gpt-test", vec![Message::user("hi")]);
        assert_eq!(json_with(&request, &settings)["reasoning_effort"], "high");

        request.effort = Some(Effort::Low);
        assert_eq!(json_with(&request, &settings)["reasoning_effort"], "low");
    }

    #[test]
    fn with_no_settings_there_is_no_effort_and_no_thinking_switch() {
        let sent = to_json(&Request::new("gpt-test", vec![Message::user("hi")]));
        assert!(sent.get("reasoning_effort").is_none(), "{sent}");
        assert!(sent.get("thinking").is_none(), "{sent}");
    }

    #[test]
    fn deepseek_thinking_with_an_effort() {
        let settings = Settings {
            thinking: Some(true),
            ..Settings::default()
        };
        let mut request = Request::new("deepseek-test", vec![Message::user("hi")]);
        request.effort = Some(Effort::Max);

        assert_eq!(
            json_with(&request, &settings),
            json!({
                "model": "deepseek-test",
                "messages": [{"role": "user", "content": "hi"}],
                "reasoning_effort": "max",
                "thinking": {"type": "enabled"},
                "stream": true,
                "stream_options": {"include_usage": true}
            })
        );
    }

    #[test]
    fn deepseek_thinking_turned_off() {
        let settings = Settings {
            thinking: Some(false),
            ..Settings::default()
        };
        let request = Request::new("deepseek-test", vec![Message::user("hi")]);
        assert_eq!(
            json_with(&request, &settings)["thinking"],
            json!({"type": "disabled"})
        );
    }

    #[test]
    fn an_effort_with_thinking_turned_off_is_rejected_before_sending() {
        let settings = Settings {
            thinking: Some(false),
            ..Settings::default()
        };
        let mut request = Request::new("deepseek-test", vec![Message::user("hi")]);
        request.effort = Some(Effort::High);
        let Err(error) = body(&request, &settings) else {
            panic!("expected the body to fail");
        };
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert!(error.message.contains("thinking off"), "{error}");
    }

    #[test]
    fn max_tokens_can_go_out_as_max_completion_tokens() {
        let settings = Settings {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..Settings::default()
        };
        let mut request = Request::new("gpt-test", vec![Message::user("hi")]);
        request.max_tokens = Some(500);

        assert_eq!(
            json_with(&request, &settings),
            json!({
                "model": "gpt-test",
                "messages": [{"role": "user", "content": "hi"}],
                "max_completion_tokens": 500,
                "stream": true,
                "stream_options": {"include_usage": true}
            })
        );
        assert_eq!(to_json(&request)["max_tokens"], 500);
        assert!(to_json(&request).get("max_completion_tokens").is_none());
    }

    // Empty text.

    #[test]
    fn a_user_message_with_only_blank_text_is_left_out() {
        let request = Request::new(
            "gpt-test",
            vec![
                Message::user("  \n"),
                Message {
                    role: Role::User,
                    content: vec![Block::Text(" ".into()), result("call_1", "12:00", false)],
                },
            ],
        );
        assert_eq!(
            to_json(&request)["messages"],
            json!([{"role": "tool", "tool_call_id": "call_1", "content": "12:00"}])
        );
    }

    #[test]
    fn an_assistant_message_with_nothing_but_blanks_is_left_out() {
        let request = Request::new(
            "gpt-test",
            vec![
                Message::user("hi"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Block::Text(" ".into()),
                        Block::Reasoning {
                            text: "\n".into(),
                            replay: None,
                        },
                    ],
                },
                Message::user("still there?"),
            ],
        );
        assert_eq!(
            to_json(&request)["messages"],
            json!([
                {"role": "user", "content": "hi"},
                {"role": "user", "content": "still there?"}
            ])
        );
    }

    #[test]
    fn blank_text_beside_a_tool_call_becomes_empty_content() {
        let request = Request::new(
            "gpt-test",
            vec![Message {
                role: Role::Assistant,
                content: vec![
                    Block::Text("  ".into()),
                    Block::ToolCall(ToolCall {
                        id: "call_1".into(),
                        name: "clock".into(),
                        arguments: "{}".into(),
                    }),
                ],
            }],
        );
        assert_eq!(to_json(&request)["messages"][0]["content"], "");
    }

    #[test]
    fn whitespace_between_text_blocks_is_kept() {
        let request = Request::new(
            "gpt-test",
            vec![Message {
                role: Role::User,
                content: vec![
                    Block::Text("Hello".into()),
                    Block::Text(" ".into()),
                    Block::Text("world".into()),
                ],
            }],
        );
        assert_eq!(to_json(&request)["messages"][0]["content"], "Hello world");
    }

    #[test]
    fn a_blank_system_prompt_is_left_out() {
        let mut request = Request::new("gpt-test", vec![Message::user("hi")]);
        request.system = Some(" ".into());
        assert_eq!(
            to_json(&request)["messages"],
            json!([{"role": "user", "content": "hi"}])
        );
    }
}
