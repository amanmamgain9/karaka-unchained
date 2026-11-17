//! A [`Request`] as the JSON body Anthropic's Messages API expects.
//!
//! The wire types borrow from the request (`&'a str`) instead of copying it:
//! the body exists only long enough to be written out, so there's nothing to
//! own.

use karaka_model::{Block, ErrorKind, Message, ModelError, Request, Role};
use serde::Serialize;
use serde_json::value::RawValue;

/// The body of `POST /v1/messages`.
#[derive(Serialize)]
pub(crate) struct Body<'a> {
    model: &'a str,
    /// Anthropic requires it, so a request that leaves it to the provider
    /// gets the provider's configured default.
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    content: Vec<WireBlock<'a>>,
}

/// One content block. `tag = "type"` writes the variant's name into the
/// object as `"type": "tool_use"` and so on, which is how Anthropic tells
/// blocks apart.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireBlock<'a> {
    Text {
        text: &'a str,
    },
    Thinking {
        thinking: &'a str,
        signature: &'a str,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        /// The model's own JSON, written out as it was.
        input: &'a RawValue,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
        #[serde(skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

#[derive(Serialize)]
struct WireTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a RawValue,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// Builds the streaming body for `request`.
///
/// Fails only if a tool's parameters aren't a JSON object, which is a bug in
/// whoever defined the tool, so it's reported before anything is sent.
pub(crate) fn body(request: &Request, default_max_tokens: u32) -> Result<Body<'_>, ModelError> {
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let input_schema = json_object(&tool.parameters).ok_or_else(|| {
                ModelError::new(
                    ErrorKind::InvalidRequest,
                    format!(
                        "tool \"{}\" has parameters that aren't a JSON object",
                        tool.name
                    ),
                )
            })?;
            Ok(WireTool {
                name: &tool.name,
                description: &tool.description,
                input_schema,
            })
        })
        .collect::<Result<_, ModelError>>()?;

    Ok(Body {
        model: &request.model,
        max_tokens: request.max_tokens.unwrap_or(default_max_tokens),
        system: request.system.as_deref(),
        messages: request.messages.iter().map(message).collect(),
        tools,
        stream: true,
    })
}

fn message(message: &Message) -> WireMessage<'_> {
    WireMessage {
        role: match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        },
        content: message.content.iter().filter_map(block).collect(),
    }
}

/// One block on the wire, or `None` for a block Anthropic can't take back.
fn block(block: &Block) -> Option<WireBlock<'_>> {
    Some(match block {
        Block::Text(text) => WireBlock::Text { text },
        // Anthropic accepts thinking back only with the signature it issued.
        // Reasoning without one came from another provider and is left out.
        Block::Reasoning { text, replay } => WireBlock::Thinking {
            thinking: text,
            signature: replay.as_deref()?,
        },
        Block::ToolCall(call) => WireBlock::ToolUse {
            id: &call.id,
            name: &call.name,
            // Broken arguments still have to go back as an object. The tool
            // already told the model they were broken, in its result.
            // A closure, not `empty_object` itself: passing the function would make
            // the whole `Option` `'static`, and the arguments are only borrowed.
            input: json_object(&call.arguments).unwrap_or_else(|| empty_object()),
        },
        Block::ToolResult(result) => WireBlock::ToolResult {
            tool_use_id: &result.call_id,
            content: &result.content,
            is_error: result.is_error,
        },
    })
}

/// `text` as raw JSON, if it is a JSON object. Checked, but not parsed into
/// values: it goes out exactly as it came in.
fn json_object(text: &str) -> Option<&RawValue> {
    let raw: &RawValue = serde_json::from_str(text).ok()?;
    raw.get().starts_with('{').then_some(raw)
}

fn empty_object() -> &'static RawValue {
    serde_json::from_str("{}").expect("`{}` is valid JSON")
}

#[cfg(test)]
mod tests {
    use karaka_model::{
        Block, ErrorKind, Message, Request, Role, ToolCall, ToolResult, ToolSchema,
    };
    use serde_json::{Value, json};

    use super::body;

    fn to_json(request: &Request) -> Value {
        serde_json::to_value(body(request, 1000).expect("the body builds")).expect("it serializes")
    }

    #[test]
    fn a_plain_question() {
        let mut request = Request::new("claude-opus-5-5", vec![Message::user("hi")]);
        request.system = Some("Be brief.".into());

        assert_eq!(
            to_json(&request),
            json!({
                "model": "claude-opus-5-5",
                "max_tokens": 1000,
                "system": "Be brief.",
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "hi"}]}
                ],
                "stream": true
            })
        );
    }

    #[test]
    fn a_tool_round_trip() {
        let mut request = Request::new(
            "claude-opus-5-5",
            vec![
                Message::user("what time is it?"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Block::Reasoning {
                            text: "check the clock".into(),
                            replay: Some("sig".into()),
                        },
                        Block::Reasoning {
                            text: "from another provider".into(),
                            replay: None,
                        },
                        Block::ToolCall(ToolCall {
                            id: "toolu_1".into(),
                            name: "clock".into(),
                            arguments: r#"{"zone": "UTC"}"#.into(),
                        }),
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![Block::ToolResult(ToolResult {
                        call_id: "toolu_1".into(),
                        content: "12:00".into(),
                        is_error: false,
                    })],
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
                "model": "claude-opus-5-5",
                "max_tokens": 64,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "what time is it?"}]},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "check the clock", "signature": "sig"},
                        {"type": "tool_use", "id": "toolu_1", "name": "clock", "input": {"zone": "UTC"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "12:00"}
                    ]}
                ],
                "tools": [
                    {"name": "clock", "description": "Tells the time.", "input_schema": {"type": "object"}}
                ],
                "stream": true
            })
        );
    }

    #[test]
    fn broken_arguments_go_back_as_an_empty_object() {
        let request = Request::new(
            "claude-opus-5-5",
            vec![Message {
                role: Role::Assistant,
                content: vec![Block::ToolCall(ToolCall {
                    id: "toolu_1".into(),
                    name: "clock".into(),
                    arguments: r#"{"zone": "#.into(),
                })],
            }],
        );
        assert_eq!(
            to_json(&request)["messages"][0]["content"][0]["input"],
            json!({})
        );
    }

    #[test]
    fn a_tool_with_broken_parameters_is_rejected_before_sending() {
        let mut request = Request::new("claude-opus-5-5", vec![Message::user("hi")]);
        request.tools.push(ToolSchema {
            name: "clock".into(),
            description: "Tells the time.".into(),
            parameters: "not json".into(),
        });
        let Err(error) = body(&request, 1000) else {
            panic!("expected the body to fail");
        };
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert!(error.message.contains("\"clock\""), "{error}");
    }
}
