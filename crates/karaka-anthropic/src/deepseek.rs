//! The DeepSeek profile: a [`Request`] as the body DeepSeek's Messages API
//! takes, written as dsh v4's `llm-deepseek` writes it.
//!
//! Each rule below follows a line of `llm-deepseek/src/`, cited where the
//! code does it. Where this profile differs from the Anthropic one:
//!
//! - Thinking is a plain switch, `{"type": "enabled"}` or
//!   `{"type": "disabled"}`, not adaptive, so it needs no beta header.
//! - While thinking is on, an effort is always sent, `high` unless set.
//!   DeepSeek has only `low`, `high` and `max`; other levels are refused
//!   before anything is sent, and so is any effort while thinking is off.
//! - No cache markers. DeepSeek caches without being asked.
//! - Reasoning always goes back as thinking, never as text. Its signature
//!   goes back only to the model that wrote it.
//! - Developer messages always go out as `{"role": "system"}` messages.
//!   A system message does too where the model takes that; for other
//!   models the newest one replaces the system prompt. Either way each one
//!   waits for the next assistant message, and must follow a user turn.
//! - A tool result's content is a list of text blocks, and an empty result
//!   is an empty list. `is_error` is always sent.
//! - Two messages next to each other with the same role are joined.
//! - A history whose tool calls and results don't pair up is refused
//!   before anything is sent.
//! - Text is left out only when it is empty, not when it is whitespace.
//! - The system prompt is always a plain string.
//!
//! dsh's developer messages can also add and remove tools, sent as
//! `tool_addition` and `tool_removal` blocks (`serialize.ts:95-96`).
//! Karaka's history has nothing to hold those, so they aren't sent; the
//! request's tools are the ones in effect.

use std::borrow::Cow;
use std::collections::HashSet;
use std::time::Duration;

use karaka_model::{Block, Effort, ErrorKind, Message, ModelError, Replay, Request, Role};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::decoder::classify;
use crate::request::{empty_object, json_object};

/// DeepSeek's public Messages API (`config.ts:115`).
pub(crate) const BASE_URL: &str = "https://api.deepseek.com/anthropic";

/// The `anthropic-version` dsh sends (`adapter.ts:144`). The Anthropic
/// profile sends a newer one, which DeepSeek may not know.
pub(crate) const API_VERSION: &str = "2023-06-01";

/// dsh's default `max_tokens` (`defaults.ts:8`). DeepSeek requires one, as
/// Anthropic does.
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 256_000;

/// How long the stream may be silent before the call is given up, as dsh
/// waits (`defaults.ts:4`). Longer than the Anthropic profile's two minutes:
/// at high effort DeepSeek can think for a while without sending anything.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// The name dsh gives its own replay data (`replay.ts:30`). It is the API
/// in the [`Origin`] of every signature this profile receives, so the
/// profile can tell its own signatures from anyone else's.
///
/// [`Origin`]: karaka_model::Origin
pub(crate) const API: &str = "deepseek-messages";

/// The provider in that origin. dsh doesn't check it (`replay.ts:53-54`),
/// and neither does this profile; it says who answered.
pub(crate) const PROVIDER: &str = "deepseek";

/// DeepSeek's switch for thinking. dsh's `thinking` setting has the same
/// two values (`config.ts:93`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeepSeekThinking {
    /// The model thinks before it answers. dsh's default: with no setting
    /// it sends `enabled` at effort `high` (`serialize.ts:147`).
    #[default]
    Enabled,
    /// The model answers straight away. No effort can be sent with it.
    Disabled,
}

/// The settings that come from the provider's config rather than from each
/// request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings {
    /// For a request that doesn't set `max_tokens`.
    pub default_max_tokens: u32,
    /// For a request that doesn't set `effort`.
    pub default_effort: Option<Effort>,
    pub thinking: DeepSeekThinking,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_max_tokens: DEFAULT_MAX_TOKENS,
            default_effort: None,
            thinking: DeepSeekThinking::default(),
        }
    }
}

/// The full URL for `base_url`. Like dsh (`messages-api.ts:14-17`), it adds
/// `/v1` only when the path doesn't already end in it, so a base URL such
/// as `https://gateway.example/v1` isn't doubled.
pub(crate) fn endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let has_version = reqwest::Url::parse(base).is_ok_and(|url| url.path().ends_with("/v1"));
    if has_version {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

// The body. The fields are in the order dsh writes them
// (`serialize.ts:152-167`), so the bytes read the same side by side.

/// The body of `POST /v1/messages`.
#[derive(Serialize)]
pub(crate) struct Body<'a> {
    model: &'a str,
    stream: bool,
    messages: Vec<WireMessage<'a>>,
    max_tokens: u32,
    thinking: Thinking,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
}

#[derive(Serialize)]
struct Thinking {
    /// `"enabled"` or `"disabled"`.
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct OutputConfig {
    effort: &'static str,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    /// `"user"`, `"assistant"` or `"system"`.
    role: &'static str,
    content: Vec<WireBlock<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireBlock<'a> {
    Text {
        /// Owned only for a system message whose text blocks are joined.
        text: Cow<'a, str>,
    },
    Thinking {
        thinking: &'a str,
        /// Left out when there is none to send.
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<&'a str>,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a RawValue,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: Vec<TextBlock<'a>>,
        is_error: bool,
    },
}

/// The only kind of block a tool result holds here. dsh also sends images
/// (`wire-types.ts:4-7`), which Karaka doesn't have yet.
#[derive(Serialize)]
struct TextBlock<'a> {
    /// Always `"text"`.
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

#[derive(Serialize)]
struct WireTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a RawValue,
}

/// Builds the streaming body for `request`.
///
/// Fails, before anything is sent, on a tool whose parameters aren't a JSON
/// object, on tool calls and results that don't pair up, and on an effort
/// DeepSeek doesn't have.
pub(crate) fn body<'a>(request: &'a Request, settings: &Settings) -> Result<Body<'a>, ModelError> {
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let input_schema = json_object(&tool.parameters).ok_or_else(|| {
                invalid(format!(
                    "tool \"{}\" has parameters that aren't a JSON object",
                    tool.name
                ))
            })?;
            Ok(WireTool {
                name: &tool.name,
                description: &tool.description,
                input_schema,
            })
        })
        .collect::<Result<_, ModelError>>()?;

    let in_history = takes_system_messages(&request.model);
    let mut messages: Vec<WireMessage<'a>> = Vec::new();
    // The request's system prompt counts as the first system message: a
    // later one that can't go in place replaces it, as dsh's
    // `historySystem` is replaced (`serialize.ts:110-115`).
    let mut system = request.system.as_deref().map(Cow::Borrowed);
    // System messages waiting for the next assistant message. dsh admits an
    // update before the user's input, and the Messages format wants it after
    // that turn (`serialize.ts:83-89`).
    let mut updates: Vec<WireMessage<'a>> = Vec::new();
    for message in &request.messages {
        let (role, content) = match message.role {
            Role::User => ("user", user(message)),
            Role::Assistant => ("assistant", assistant(message, &request.model)?),
            // Always in place, whatever the model (`serialize.ts:91-101`).
            Role::Developer => {
                let content = developer(message);
                if !content.is_empty() {
                    updates.push(WireMessage {
                        role: "system",
                        content,
                    });
                }
                continue;
            }
            Role::System => {
                // dsh joins a system message's blocks with nothing between
                // them (`serialize.ts:109`).
                let text = joined_text(message);
                // The first message can't be a system update, so a system
                // message before anything else replaces the prompt too.
                if in_history && !messages.is_empty() {
                    if text.is_empty() {
                        return Err(invalid("a system message in the history is empty"));
                    }
                    updates.push(WireMessage {
                        role: "system",
                        content: vec![WireBlock::Text { text }],
                    });
                } else {
                    system = Some(text);
                }
                continue;
            }
        };
        // A message with nothing left in it is skipped (`serialize.ts:123`).
        if content.is_empty() {
            continue;
        }
        if role == "assistant" {
            release(&mut updates, &mut messages)?;
        }
        // One next to a message with the same role joins it
        // (`serialize.ts:126-128`). Skipping a message can bring two
        // together, and Karaka's history can hold them anyway.
        match messages.last_mut() {
            Some(previous) if previous.role == role => previous.content.extend(content),
            _ => messages.push(WireMessage { role, content }),
        }
    }
    release(&mut updates, &mut messages)?;
    check_tool_history(&mut messages)?;
    let (thinking, output_config) = thinking_and_effort(request, settings)?;

    Ok(Body {
        model: &request.model,
        stream: true,
        messages,
        // No clamp: dsh sends what it is given (`serialize.ts:154`).
        max_tokens: request.max_tokens.unwrap_or(settings.default_max_tokens),
        thinking,
        output_config,
        // Only an empty prompt is left out; dsh keeps one that is only
        // whitespace (`serialize.ts:151`).
        system: system.filter(|text| !text.is_empty()),
        tools,
    })
}

/// Whether `model` takes system messages in place. dsh's default catalog
/// says so only for `deepseek-flash` (`models.ts:7-14`). dsh lets a
/// deployment say so for other models in its config (`config.ts:80`);
/// Karaka has no setting for that yet.
fn takes_system_messages(model: &str) -> bool {
    model == "deepseek-flash"
}

/// Moves the waiting system messages onto the end of `messages`. They must
/// follow a user turn, which here includes tool results
/// (`serialize.ts:85-89`).
fn release<'a>(
    updates: &mut Vec<WireMessage<'a>>,
    messages: &mut Vec<WireMessage<'a>>,
) -> Result<(), ModelError> {
    if updates.is_empty() {
        return Ok(());
    }
    if messages.last().map(|message| message.role) != Some("user") {
        return Err(invalid(
            "a system message must come after a user turn or tool results",
        ));
    }
    messages.append(updates);
    Ok(())
}

/// The text blocks of `message` joined with nothing between them, borrowed
/// when there is only one.
fn joined_text(message: &Message) -> Cow<'_, str> {
    let texts: Vec<&str> = message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    match texts.as_slice() {
        [one] => Cow::Borrowed(one),
        _ => Cow::Owned(texts.concat()),
    }
}

/// A developer message's text blocks, leaving out empty ones
/// (`serialize.ts:94`).
fn developer(message: &Message) -> Vec<WireBlock<'_>> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Text(text) if !text.is_empty() => Some(WireBlock::Text {
                text: Cow::Borrowed(text),
            }),
            _ => None,
        })
        .collect()
}

/// A user message's blocks. Reasoning and tool calls belong to the model,
/// so they are dropped here (`serialize.ts:67`).
fn user(message: &Message) -> Vec<WireBlock<'_>> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Text(text) if text.is_empty() => None,
            Block::Text(text) => Some(WireBlock::Text {
                text: Cow::Borrowed(text),
            }),
            Block::ToolResult(result) => Some(WireBlock::ToolResult {
                tool_use_id: &result.call_id,
                // An empty result stays empty, rather than gaining text the
                // tool never wrote (`serialize.ts:66,121`).
                content: if result.content.is_empty() {
                    Vec::new()
                } else {
                    vec![TextBlock {
                        kind: "text",
                        text: &result.content,
                    }]
                },
                is_error: result.is_error,
            }),
            Block::Reasoning { .. } | Block::ToolCall(_) => None,
        })
        .collect()
}

/// An assistant message's blocks, for a request to `model`
/// (`serialize.ts:26-39`).
fn assistant<'a>(message: &'a Message, model: &str) -> Result<Vec<WireBlock<'a>>, ModelError> {
    let mut blocks = Vec::new();
    for block in &message.content {
        blocks.push(match block {
            Block::Text(text) if text.is_empty() => continue,
            Block::Text(text) => WireBlock::Text {
                text: Cow::Borrowed(text),
            },
            // Always thinking, even from another provider or with no text,
            // because DeepSeek reads thinking it didn't sign. Only the
            // signature depends on where the reasoning came from.
            Block::Reasoning { text, replay } => WireBlock::Thinking {
                thinking: text,
                signature: replay
                    .as_ref()
                    .filter(|replay| is_own(replay, model))
                    .map(|replay| replay.signature.as_str()),
            },
            // Arguments that aren't a JSON object go back as an empty one
            // (`serialize.ts:16-24`).
            Block::ToolCall(call) => WireBlock::ToolUse {
                id: &call.id,
                name: &call.name,
                input: json_object(&call.arguments).unwrap_or_else(|| empty_object()),
            },
            // dsh can't represent this either (`serialize.ts:36`).
            Block::ToolResult(_) => {
                return Err(invalid(
                    "DeepSeek can't take a tool result in an assistant message",
                ));
            }
        });
    }
    Ok(blocks)
}

/// Refuses tool calls and results that don't pair up, which DeepSeek would
/// refuse anyway, and puts each message's results before its text
/// (`serialize.ts:131-146`).
///
/// The rule: each assistant message's tool calls are answered, every one
/// of them, by the message that comes right after it, and every result
/// answers a call.
fn check_tool_history<'a>(messages: &mut [WireMessage<'a>]) -> Result<(), ModelError> {
    // The calls still waiting for a result.
    let mut waiting: HashSet<&'a str> = HashSet::new();
    for message in messages.iter_mut() {
        // System messages take no part in pairing calls with results.
        if message.role == "system" {
            continue;
        }
        if message.role == "assistant" {
            waiting.clear();
            for block in &message.content {
                if let WireBlock::ToolUse { id, .. } = block
                    && !waiting.insert(id)
                {
                    return Err(invalid(format!(
                        "two tool calls in one message have the id \"{id}\""
                    )));
                }
            }
        } else {
            for block in &message.content {
                if let WireBlock::ToolResult { tool_use_id, .. } = block
                    && !waiting.remove(tool_use_id)
                {
                    return Err(invalid(format!(
                        "the tool result for \"{tool_use_id}\" answers no call"
                    )));
                }
            }
            if !waiting.is_empty() {
                return Err(invalid(
                    "tool calls must be answered in the message right after them",
                ));
            }
            // A stable sort, so results and text each keep their own order.
            message
                .content
                .sort_by_key(|block| !matches!(block, WireBlock::ToolResult { .. }));
        }
    }
    if waiting.is_empty() {
        Ok(())
    } else {
        Err(invalid(
            "the history ends with tool calls that have no results",
        ))
    }
}

/// The thinking switch and the effort, checked as dsh checks them
/// (`serialize.ts:147-150`).
///
/// The effort is the request's, then the config's, then `high`. Karaka has
/// no effort called "off": thinking that is off is the config's
/// [`DeepSeekThinking::Disabled`], and then no effort can be set at all.
fn thinking_and_effort(
    request: &Request,
    settings: &Settings,
) -> Result<(Thinking, Option<OutputConfig>), ModelError> {
    let effort = request.effort.or(settings.default_effort);
    match settings.thinking {
        DeepSeekThinking::Disabled => match effort {
            Some(effort) => Err(invalid(format!(
                "the effort is \"{}\", but DeepSeek takes no effort with thinking disabled",
                effort_label(effort)
            ))),
            None => Ok((Thinking { kind: "disabled" }, None)),
        },
        DeepSeekThinking::Enabled => {
            let effort = effort.unwrap_or(Effort::High);
            let name = effort_name(effort).ok_or_else(|| {
                invalid(format!(
                    "DeepSeek has no effort \"{}\"; it takes low, high or max",
                    effort_label(effort)
                ))
            })?;
            Ok((
                Thinking { kind: "enabled" },
                Some(OutputConfig { effort: name }),
            ))
        }
    }
}

/// DeepSeek's name for each effort it has (`config.ts:94`,
/// `wire-types.ts:31`).
///
/// | Effort   | `output_config.effort` |
/// |----------|------------------------|
/// | `Low`    | `"low"`                |
/// | `Medium` | refused                |
/// | `High`   | `"high"`, the default  |
/// | `XHigh`  | refused                |
/// | `Max`    | `"max"`                |
///
/// `Medium` and `XHigh` aren't rounded to a neighbour: dsh refuses them
/// (`serialize.spec.ts`, "refuses unsupported effort"), and rounding would
/// hide a setting that does nothing.
pub(crate) fn effort_name(effort: Effort) -> Option<&'static str> {
    match effort {
        Effort::Low => Some("low"),
        Effort::High => Some("high"),
        Effort::Max => Some("max"),
        Effort::Medium | Effort::XHigh => None,
    }
}

/// How an effort is written in config files, for error messages.
pub(crate) fn effort_label(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

fn invalid(message: impl Into<String>) -> ModelError {
    ModelError::new(ErrorKind::InvalidRequest, message)
}

// Reasoning replay.

/// Whether this profile can send `replay`'s signature with a request to
/// `model`. dsh checks only its own kind of replay data and the model
/// (`replay.ts:53-54,63`). An empty signature still counts: dsh sends one
/// back as it got it (`serialize.spec.ts`, "keeps valid cross-model and
/// foreign history quiet").
fn is_own(replay: &Replay, model: &str) -> bool {
    replay.origin.api == API && replay.origin.model == model
}

// Errors.

/// An error as DeepSeek sends it, in a failed response or in the stream.
/// Unlike Anthropic's, any part may be missing, and it can carry a `code`
/// such as `insufficient_balance` (`transport.ts:22-27`). A part that isn't
/// a string counts as missing, as in dsh.
#[derive(Default, Deserialize)]
pub(crate) struct WireError {
    #[serde(rename = "type")]
    kind: Option<Value>,
    code: Option<Value>,
    message: Option<Value>,
}

impl WireError {
    /// The failure this error means, classified on the shared rules, with
    /// the code read as well. `status` is `None` for an error in the stream.
    pub(crate) fn into_model_error(self, status: Option<u16>) -> ModelError {
        let text = |value: Option<Value>| match value {
            Some(Value::String(text)) => Some(text),
            _ => None,
        };
        let kind = text(self.kind);
        let code = text(self.code);
        // dsh's wording when DeepSeek gives no message (`transport.ts:25`).
        let message = text(self.message).unwrap_or_else(|| match status {
            Some(status) => format!("DeepSeek's request failed (HTTP {status})"),
            None => "DeepSeek's request failed (stream error)".to_owned(),
        });
        let detail = match &code {
            Some(code) => format!("{code} {message}"),
            None => message.clone(),
        };
        ModelError::new(classify(kind.as_deref(), &detail, status), message)
    }
}

/// An event in the stream, read only far enough to see an error in it.
#[derive(Deserialize)]
pub(crate) struct ErrorEvent {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) error: WireError,
}

#[cfg(test)]
mod tests {
    use karaka_model::{
        Block, Effort, ErrorKind, Message, Origin, Replay, Request, Role, ToolCall, ToolResult,
        ToolSchema,
    };
    use serde_json::{Value, json};

    use super::{
        DEFAULT_MAX_TOKENS, DeepSeekThinking, ErrorEvent, Settings, WireError, body, endpoint,
    };

    /// dsh's test model (`tests/helpers.ts:21`).
    const MODEL: &str = "deepseek-v4-flash";

    fn to_json(request: &Request) -> Value {
        json_with(request, &Settings::default())
    }

    fn json_with(request: &Request, settings: &Settings) -> Value {
        serde_json::to_value(body(request, settings).expect("the body builds"))
            .expect("it serializes")
    }

    /// Just the messages, for tests about how history translates.
    fn messages(history: Vec<Message>) -> Value {
        to_json(&Request::new(MODEL, history))["messages"].clone()
    }

    fn refusal(request: &Request, settings: &Settings) -> String {
        let Err(error) = body(request, settings) else {
            panic!("expected the body to be refused");
        };
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        error.message
    }

    fn user(content: Vec<Block>) -> Message {
        Message {
            role: Role::User,
            content,
        }
    }

    fn assistant(content: Vec<Block>) -> Message {
        Message {
            role: Role::Assistant,
            content,
        }
    }

    fn text(text: &str) -> Block {
        Block::Text(text.into())
    }

    fn reasoning(text: &str, replay: Option<Replay>) -> Block {
        Block::Reasoning {
            text: text.into(),
            replay,
        }
    }

    /// A signature this profile received from `model`.
    fn signed(model: &str, signature: &str) -> Option<Replay> {
        Some(Replay::new(
            Origin::new("deepseek-messages", "deepseek", model),
            signature,
        ))
    }

    /// A signature Anthropic's profile received from `model`.
    fn anthropic_signed(model: &str, signature: &str) -> Option<Replay> {
        Some(Replay::new(
            Origin::new("anthropic-messages", "anthropic", model),
            signature,
        ))
    }

    /// The model dsh's default catalog lets take system messages in place
    /// (`models.ts:12`).
    const IN_HISTORY: &str = "deepseek-flash";

    /// Messages for a model that takes system messages in place, as dsh's
    /// `nativeBody` builds them (`serialize.spec.ts:23-24`).
    fn native(history: Vec<Message>) -> Value {
        to_json(&Request::new(IN_HISTORY, history))["messages"].clone()
    }

    fn developer(texts: &[&str]) -> Message {
        Message {
            role: Role::Developer,
            content: texts.iter().map(|t| text(t)).collect(),
        }
    }

    fn answer(text_: &str) -> Message {
        assistant(vec![text(text_)])
    }

    /// dsh's `call` (`serialize.spec.ts:16`).
    fn call(id: &str) -> Block {
        Block::ToolCall(ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: r#"{"path":"a"}"#.into(),
        })
    }

    fn result(id: &str, content: &str, is_error: bool) -> Block {
        Block::ToolResult(ToolResult {
            call_id: id.into(),
            content: content.into(),
            is_error,
        })
    }

    fn read_tool() -> ToolSchema {
        ToolSchema {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: r#"{"type": "object"}"#.into(),
        }
    }

    fn thinking_off() -> Settings {
        Settings {
            thinking: DeepSeekThinking::Disabled,
            ..Settings::default()
        }
    }

    // Thinking and effort.

    #[test]
    fn thinking_is_on_at_high_effort_unless_set() {
        assert_eq!(
            to_json(&Request::new(MODEL, vec![Message::user("hello")])),
            json!({
                "model": MODEL,
                "stream": true,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "hello"}]}
                ],
                "max_tokens": 256000,
                "thinking": {"type": "enabled"},
                "output_config": {"effort": "high"}
            })
        );
    }

    #[test]
    fn thinking_disabled_sends_no_effort() {
        assert_eq!(
            json_with(
                &Request::new(MODEL, vec![Message::user("hello")]),
                &thinking_off()
            ),
            json!({
                "model": MODEL,
                "stream": true,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "hello"}]}
                ],
                "max_tokens": 256000,
                "thinking": {"type": "disabled"}
            })
        );
    }

    /// dsh's "maps reasoning effort %s", for the levels Karaka has.
    #[test]
    fn low_high_and_max_keep_their_names() {
        for (effort, name) in [
            (Effort::Low, "low"),
            (Effort::High, "high"),
            (Effort::Max, "max"),
        ] {
            let mut request = Request::new(MODEL, vec![Message::user("hello")]);
            request.effort = Some(effort);
            let sent = to_json(&request);
            assert_eq!(sent["thinking"], json!({"type": "enabled"}));
            assert_eq!(sent["output_config"], json!({"effort": name}));
        }
    }

    /// dsh's "refuses unsupported effort": `medium` throws before sending.
    #[test]
    fn medium_and_xhigh_are_refused_before_sending() {
        for (effort, name) in [(Effort::Medium, "medium"), (Effort::XHigh, "xhigh")] {
            let mut request = Request::new(MODEL, vec![Message::user("hello")]);
            request.effort = Some(effort);
            assert_eq!(
                refusal(&request, &Settings::default()),
                format!("DeepSeek has no effort \"{name}\"; it takes low, high or max")
            );
        }
    }

    /// dsh refuses `high` when the config disables thinking.
    #[test]
    fn any_effort_with_thinking_disabled_is_refused() {
        let mut request = Request::new(MODEL, vec![Message::user("hello")]);
        request.effort = Some(Effort::High);
        assert_eq!(
            refusal(&request, &thinking_off()),
            "the effort is \"high\", but DeepSeek takes no effort with thinking disabled"
        );

        let settings = Settings {
            default_effort: Some(Effort::Low),
            ..thinking_off()
        };
        let request = Request::new(MODEL, vec![Message::user("hello")]);
        refusal(&request, &settings);
    }

    #[test]
    fn the_configs_effort_applies_when_the_request_sets_none() {
        let settings = Settings {
            default_effort: Some(Effort::Max),
            ..Settings::default()
        };
        let mut request = Request::new(MODEL, vec![Message::user("hello")]);
        assert_eq!(
            json_with(&request, &settings)["output_config"],
            json!({"effort": "max"})
        );

        request.effort = Some(Effort::Low);
        assert_eq!(
            json_with(&request, &settings)["output_config"],
            json!({"effort": "low"})
        );
    }

    // `max_tokens`.

    #[test]
    fn max_tokens_comes_from_the_config_unless_the_request_sets_it() {
        let mut request = Request::new(MODEL, vec![Message::user("hello")]);
        assert_eq!(to_json(&request)["max_tokens"], DEFAULT_MAX_TOKENS);

        // Play This Life's setting.
        let settings = Settings {
            default_max_tokens: 393_216,
            ..Settings::default()
        };
        assert_eq!(json_with(&request, &settings)["max_tokens"], 393_216);

        // dsh's "explicit output cap".
        request.max_tokens = Some(123);
        assert_eq!(json_with(&request, &settings)["max_tokens"], 123);
    }

    // Tool results.

    /// dsh's "preserves empty results without inventing model-visible
    /// output".
    #[test]
    fn an_empty_tool_result_goes_back_as_an_empty_list() {
        assert_eq!(
            messages(vec![
                Message::user("hello"),
                assistant(vec![call("a")]),
                user(vec![result("a", "", false)]),
            ])[2],
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a", "content": [], "is_error": false}
            ]})
        );
    }

    /// dsh's "groups parallel results before ordinary text and keeps tool
    /// failure content".
    #[test]
    fn results_come_before_text_and_a_failure_says_so() {
        assert_eq!(
            messages(vec![
                Message::user("hello"),
                assistant(vec![call("a"), call("b")]),
                user(vec![
                    text("follow-up"),
                    result("a", "result", false),
                    result("b", "permission denied", true),
                ]),
            ]),
            json!([
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "a", "name": "read", "input": {"path": "a"}},
                    {"type": "tool_use", "id": "b", "name": "read", "input": {"path": "a"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "a",
                     "content": [{"type": "text", "text": "result"}], "is_error": false},
                    {"type": "tool_result", "tool_use_id": "b",
                     "content": [{"type": "text", "text": "permission denied"}], "is_error": true},
                    {"type": "text", "text": "follow-up"}
                ]}
            ])
        );
    }

    /// dsh's "uses empty input for malformed or non-object historical tool
    /// arguments".
    #[test]
    fn arguments_that_arent_an_object_go_back_as_an_empty_one() {
        for arguments in ["{", "", "[]", "null", "42", "true", "\"text\""] {
            let sent = messages(vec![
                Message::user("hello"),
                assistant(vec![Block::ToolCall(ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: arguments.into(),
                })]),
                user(vec![result("a", "Invalid arguments", true)]),
            ]);
            assert_eq!(sent[1]["content"][0]["input"], json!({}), "{arguments}");
        }
    }

    // Refused tool history.

    /// dsh's "rejects unmatched or duplicated tool history", case by case.
    #[test]
    fn tool_history_that_doesnt_pair_up_is_refused() {
        let cases = [
            (
                vec![user(vec![result("a", "result", false)])],
                "the tool result for \"a\" answers no call",
            ),
            (
                vec![assistant(vec![call("a")])],
                "the history ends with tool calls that have no results",
            ),
            (
                vec![assistant(vec![call("a")]), Message::user("hello")],
                "tool calls must be answered in the message right after them",
            ),
            (
                vec![
                    assistant(vec![call("a"), call("a")]),
                    user(vec![result("a", "result", false)]),
                ],
                "two tool calls in one message have the id \"a\"",
            ),
            (
                vec![
                    assistant(vec![call("a")]),
                    user(vec![
                        result("a", "result", false),
                        result("a", "result", false),
                    ]),
                ],
                "the tool result for \"a\" answers no call",
            ),
        ];
        for (history, expected) in cases {
            let request = Request::new(MODEL, history);
            assert_eq!(refusal(&request, &Settings::default()), expected);
        }
    }

    #[test]
    fn a_tool_result_in_an_assistant_message_is_refused() {
        let request = Request::new(MODEL, vec![assistant(vec![result("a", "x", false)])]);
        assert_eq!(
            refusal(&request, &Settings::default()),
            "DeepSeek can't take a tool result in an assistant message"
        );
    }

    // Reasoning replay.

    /// dsh's "preserves own signed thinking, omits absent signatures and
    /// validates durable metadata".
    #[test]
    fn its_own_signature_goes_back_only_to_the_model_that_wrote_it() {
        let history = vec![
            Message::user("hello"),
            assistant(vec![reasoning("", signed(MODEL, "signed")), text("answer")]),
            Message::user("hello"),
        ];
        assert_eq!(
            messages(history.clone())[1]["content"],
            json!([
                {"type": "thinking", "thinking": "", "signature": "signed"},
                {"type": "text", "text": "answer"}
            ])
        );

        // Another model's request gets the thinking, but not the signature.
        let sent = to_json(&Request::new("different-model", history));
        assert_eq!(
            sent["messages"][1]["content"][0],
            json!({"type": "thinking", "thinking": ""})
        );
    }

    /// Reasoning from another provider stays thinking, not text, unlike in
    /// the Anthropic profile. An Anthropic signature isn't DeepSeek's, so it
    /// isn't sent, even when the model's name is the same.
    #[test]
    fn foreign_reasoning_goes_back_as_unsigned_thinking() {
        assert_eq!(
            messages(vec![assistant(vec![
                reasoning("foreign thought", None),
                reasoning(
                    "Anthropic thought",
                    anthropic_signed(MODEL, "EqQBCkgIAhABGAIiQL")
                ),
                reasoning("", None),
            ])])[0]["content"],
            json!([
                {"type": "thinking", "thinking": "foreign thought"},
                {"type": "thinking", "thinking": "Anthropic thought"},
                {"type": "thinking", "thinking": ""}
            ])
        );
    }

    /// dsh's "keeps valid cross-model and foreign history quiet": an empty
    /// signature of its own still goes back.
    #[test]
    fn an_empty_signature_of_its_own_goes_back() {
        assert_eq!(
            messages(vec![assistant(vec![reasoning("think", signed(MODEL, ""))])])[0]["content"],
            json!([{"type": "thinking", "thinking": "think", "signature": ""}])
        );
    }

    /// dsh's "keeps durable content when replay degrades", checked against
    /// its snapshot `tests/expected/degraded-replay.json`, for its second
    /// case: the request names a different model than the signature's.
    #[test]
    fn a_signature_from_another_deepseek_model_degrades_to_unsigned_thinking() {
        assert_eq!(
            messages(vec![
                Message::user("hello"),
                assistant(vec![
                    reasoning("Read the file.", signed("different-model", "do-not-send")),
                    text("Checking a."),
                    call("a"),
                ]),
                user(vec![result("a", "result", false)]),
            ]),
            json!([
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "Read the file."},
                    {"type": "text", "text": "Checking a."},
                    {"type": "tool_use", "id": "a", "name": "read", "input": {"path": "a"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "a",
                     "content": [{"type": "text", "text": "result"}], "is_error": false}
                ]}
            ])
        );
    }

    // System and developer messages.

    /// dsh's "collects leading system text", second half: for a model that
    /// doesn't take them in place, a later system message replaces the
    /// prompt.
    #[test]
    fn a_later_system_message_replaces_the_prompt_where_it_cant_go_in_place() {
        let mut request = Request::new(
            MODEL,
            vec![
                Message::user("first"),
                answer("one"),
                Message::system("B"),
                Message::user("second"),
            ],
        );
        request.system = Some("A".into());
        assert_eq!(
            to_json(&request),
            json!({
                "model": MODEL,
                "stream": true,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "first"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "one"}]},
                    {"role": "user", "content": [{"type": "text", "text": "second"}]}
                ],
                "max_tokens": 256000,
                "thinking": {"type": "enabled"},
                "output_config": {"effort": "high"},
                "system": "B"
            })
        );
    }

    /// dsh's "uses the latest complete system snapshot without changing
    /// tool history": every system message replaces the one before.
    #[test]
    fn the_newest_system_message_is_the_prompt_where_it_cant_go_in_place() {
        let conversation = vec![
            Message::user("hello"),
            assistant(vec![call("a")]),
            user(vec![result("a", "result", false)]),
            answer("done"),
            Message::user("continue"),
        ];
        let mut history = conversation.clone();
        history.insert(2, Message::system("intermediate"));
        history.insert(5, Message::system("current"));
        let mut request = Request::new(MODEL, history);
        request.system = Some("obsolete".into());

        let mut expected = to_json(&Request::new(MODEL, conversation));
        expected["system"] = json!("current");
        assert_eq!(to_json(&request), expected);
    }

    /// dsh's "clears earlier prompt snapshots with empty content".
    #[test]
    fn an_empty_system_message_clears_the_prompt_where_it_cant_go_in_place() {
        let mut request = Request::new(MODEL, vec![Message::user("hello"), Message::system("")]);
        request.system = Some("old".into());
        assert!(to_json(&request).get("system").is_none());
    }

    /// The test the brief asks for: system "A", user, assistant, system
    /// "B", user, for a model that takes system messages in place. "A"
    /// stays the prompt, and "B" goes out after the user turn that follows
    /// it, as dsh's "keeps the original top-level prompt and cached prefix
    /// while appending native system updates" expects.
    #[test]
    fn a_later_system_message_goes_in_place_where_the_model_takes_it() {
        let mut request = Request::new(
            IN_HISTORY,
            vec![
                Message::user("first"),
                answer("one"),
                Message::system("B"),
                Message::user("second"),
            ],
        );
        request.system = Some("A".into());
        assert_eq!(
            to_json(&request),
            json!({
                "model": IN_HISTORY,
                "stream": true,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "first"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "one"}]},
                    {"role": "user", "content": [{"type": "text", "text": "second"}]},
                    {"role": "system", "content": [{"type": "text", "text": "B"}]}
                ],
                "max_tokens": 256000,
                "thinking": {"type": "enabled"},
                "output_config": {"effort": "high"},
                "system": "A"
            })
        );
    }

    /// dsh's "places system updates after all parallel tool results and
    /// before the next assistant". Derivation has already put the two
    /// updates after the results, where pi-ai's transform puts them.
    #[test]
    fn system_messages_wait_for_the_next_assistant_message() {
        assert_eq!(
            native(vec![
                Message::user("hello"),
                assistant(vec![call("a"), call("b")]),
                user(vec![
                    result("a", "result", false),
                    result("b", "result", false)
                ]),
                Message::system("first update"),
                Message::system("second update"),
                Message::user("more input"),
                answer("done"),
            ]),
            json!([
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "a", "name": "read", "input": {"path": "a"}},
                    {"type": "tool_use", "id": "b", "name": "read", "input": {"path": "a"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "a",
                     "content": [{"type": "text", "text": "result"}], "is_error": false},
                    {"type": "tool_result", "tool_use_id": "b",
                     "content": [{"type": "text", "text": "result"}], "is_error": false},
                    {"type": "text", "text": "more input"}
                ]},
                {"role": "system", "content": [{"type": "text", "text": "first update"}]},
                {"role": "system", "content": [{"type": "text", "text": "second update"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "done"}]}
            ])
        );
    }

    /// dsh's "does not flush a system update at an omitted empty assistant
    /// message".
    #[test]
    fn an_empty_assistant_message_doesnt_release_a_system_message() {
        assert_eq!(
            native(vec![
                Message::user("hello"),
                Message::system("updated"),
                answer(""),
                Message::user("hello"),
                answer("answer"),
            ]),
            json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "text", "text": "hello"}
                ]},
                {"role": "system", "content": [{"type": "text", "text": "updated"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "answer"}]}
            ])
        );
    }

    /// dsh's "accepts native trailing updates without a top-level prompt and
    /// rejects unrepresentable positions", and "rejects native system
    /// updates when their user input is omitted".
    #[test]
    fn a_system_message_must_follow_a_user_turn() {
        assert_eq!(
            native(vec![Message::user("hello"), Message::system("update")]),
            json!([
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "system", "content": [{"type": "text", "text": "update"}]}
            ])
        );
        let after_an_answer = [
            vec![
                Message::user("hello"),
                answer("done"),
                Message::system("update"),
            ],
            // The user turn after it has nothing left in it.
            vec![
                Message::user("hello"),
                answer("answer"),
                Message::system("updated"),
                user(vec![reasoning("child reasoning", None)]),
                answer("next answer"),
            ],
        ];
        for history in after_an_answer {
            assert_eq!(
                refusal(&Request::new(IN_HISTORY, history), &Settings::default()),
                "a system message must come after a user turn or tool results"
            );
        }
        assert_eq!(
            refusal(
                &Request::new(
                    IN_HISTORY,
                    vec![Message::user("hello"), Message::system("")]
                ),
                &Settings::default()
            ),
            "a system message in the history is empty"
        );
    }

    /// dsh's "places developer tool changes as a system update after the
    /// preceding user turn", for the text: Karaka has no tool changes to
    /// send. A developer message goes in place for any model.
    #[test]
    fn a_developer_message_goes_in_place_after_the_user_turn() {
        assert_eq!(
            messages(vec![
                Message::user("hello"),
                developer(&["", "search replaces fetch"]),
                answer("ok"),
            ]),
            json!([
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "system", "content": [{"type": "text", "text": "search replaces fetch"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}]}
            ])
        );
        assert_eq!(
            messages(vec![Message::user("hello"), developer(&[""])]),
            json!([{"role": "user", "content": [{"type": "text", "text": "hello"}]}])
        );
        let roles = messages(vec![developer(&["note"]), Message::user("hello")]);
        assert_eq!(roles[0]["role"], "user");
        assert_eq!(roles[1]["role"], "system");
        assert_eq!(
            refusal(
                &Request::new(MODEL, vec![developer(&["note"])]),
                &Settings::default()
            ),
            "a system message must come after a user turn or tool results"
        );
    }

    /// Reasoning that went to Anthropic and back keeps DeepSeek's own
    /// signature for DeepSeek, and Anthropic's goes as unsigned thinking.
    #[test]
    fn signatures_survive_a_trip_through_anthropic() {
        let sent = messages(vec![
            Message::user("hello"),
            assistant(vec![reasoning("DeepSeek thought", signed(MODEL, "ds-sig"))]),
            Message::user("again"),
            assistant(vec![reasoning(
                "Claude thought",
                anthropic_signed("claude-opus-5-5", "claude-sig"),
            )]),
            Message::user("once more"),
        ]);
        assert_eq!(
            sent[1]["content"],
            json!([{"type": "thinking", "thinking": "DeepSeek thought", "signature": "ds-sig"}])
        );
        assert_eq!(
            sent[3]["content"],
            json!([{"type": "thinking", "thinking": "Claude thought"}])
        );
    }

    // How history translates.

    /// dsh's "omits saved empty assistant text without changing tool ids or
    /// thinking signatures": empty text goes, but whitespace stays.
    #[test]
    fn only_empty_text_is_dropped() {
        assert_eq!(
            messages(vec![
                user(vec![text(""), text("hello"), text(" ")]),
                assistant(vec![text(""), text(" ")]),
            ]),
            json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "text", "text": " "}
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": " "}]}
            ])
        );
    }

    /// dsh's "omits assistant-only blocks from user input while preserving
    /// text and durable content".
    #[test]
    fn reasoning_and_tool_calls_are_dropped_from_user_messages() {
        assert_eq!(
            messages(vec![user(vec![
                text("Background subagent finished.\n"),
                reasoning("child reasoning", None),
                call("child-call"),
                text("  child answer  "),
            ])]),
            json!([{"role": "user", "content": [
                {"type": "text", "text": "Background subagent finished.\n"},
                {"type": "text", "text": "  child answer  "}
            ]}])
        );
    }

    /// dsh's "does not flush a system update at an omitted empty assistant
    /// message", without the system update: the empty message goes, and
    /// the two user messages either side of it become one.
    #[test]
    fn messages_with_the_same_role_are_joined() {
        assert_eq!(
            messages(vec![
                Message::user("hello"),
                assistant(vec![text("")]),
                Message::user("hello"),
                assistant(vec![text("answer")]),
            ]),
            json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "text", "text": "hello"}
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "answer"}]}
            ])
        );
    }

    #[test]
    fn an_empty_system_prompt_is_left_out() {
        let mut request = Request::new(MODEL, vec![Message::user("hello")]);
        request.system = Some(String::new());
        assert!(to_json(&request).get("system").is_none());

        request.system = Some(" ".into());
        assert_eq!(to_json(&request)["system"], " ");
    }

    /// How many cache markers a body carries, wherever they are.
    fn markers(body: &Value) -> usize {
        match body {
            Value::Object(map) => {
                usize::from(map.contains_key("cache_control"))
                    + map.values().map(markers).sum::<usize>()
            }
            Value::Array(items) => items.iter().map(markers).sum(),
            _ => 0,
        }
    }

    /// A system prompt, a tool and one round of a tool call, with thinking
    /// on at effort max, as Play This Life asks: the whole body.
    #[test]
    fn a_tool_round_at_max_effort_has_no_cache_markers() {
        let mut request = Request::new(
            MODEL,
            vec![
                Message::user("what is in a?"),
                assistant(vec![
                    reasoning("Read the file.", signed(MODEL, "sig")),
                    call("a"),
                ]),
                user(vec![result("a", "hello", false)]),
            ],
        );
        request.system = Some("Be brief.".into());
        request.tools = vec![read_tool()];
        let settings = Settings {
            default_max_tokens: 393_216,
            default_effort: Some(Effort::Max),
            thinking: DeepSeekThinking::Enabled,
        };

        let sent = json_with(&request, &settings);
        assert_eq!(markers(&sent), 0);
        assert_eq!(
            sent,
            json!({
                "model": MODEL,
                "stream": true,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "what is in a?"}]},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "Read the file.", "signature": "sig"},
                        {"type": "tool_use", "id": "a", "name": "read", "input": {"path": "a"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "a",
                         "content": [{"type": "text", "text": "hello"}], "is_error": false}
                    ]}
                ],
                "max_tokens": 393216,
                "thinking": {"type": "enabled"},
                "output_config": {"effort": "max"},
                "system": "Be brief.",
                "tools": [
                    {"name": "read", "description": "Read a file", "input_schema": {"type": "object"}}
                ]
            })
        );
    }

    #[test]
    fn a_tool_with_broken_parameters_is_rejected_before_sending() {
        let mut request = Request::new(MODEL, vec![Message::user("hello")]);
        request.tools.push(ToolSchema {
            parameters: "not json".into(),
            ..read_tool()
        });
        assert_eq!(
            refusal(&request, &Settings::default()),
            "tool \"read\" has parameters that aren't a JSON object"
        );
    }

    // The endpoint.

    /// dsh's "resolves the Messages endpoint from %s" (`adapter.spec.ts`).
    #[test]
    fn v1_is_added_unless_the_path_ends_in_it() {
        let cases = [
            (
                "https://provider.example",
                "https://provider.example/v1/messages",
            ),
            (
                "https://provider.example/v1/",
                "https://provider.example/v1/messages",
            ),
            (
                "https://provider.example/v1beta",
                "https://provider.example/v1beta/v1/messages",
            ),
            (
                "https://provider.example/v2",
                "https://provider.example/v2/v1/messages",
            ),
            (
                "https://provider.example/anthropic",
                "https://provider.example/anthropic/v1/messages",
            ),
            (
                "https://v1.provider.example",
                "https://v1.provider.example/v1/messages",
            ),
        ];
        for (base, expected) in cases {
            assert_eq!(endpoint(base), expected, "{base}");
        }
    }

    // Errors.

    fn classify(error: Value, status: Option<u16>) -> ErrorKind {
        let error: WireError = serde_json::from_value(error).expect("the error reads");
        error.into_model_error(status).kind
    }

    /// dsh's "classifies status %s and error %j" (`stream.spec.ts:122-131`),
    /// and the `insufficient_balance` code from `stream.spec.ts:12`.
    #[test]
    fn errors_are_classified_as_in_dsh() {
        let cases = [
            (Some(401), json!({}), ErrorKind::Auth),
            (Some(403), json!({}), ErrorKind::Auth),
            (Some(402), json!({}), ErrorKind::Quota),
            (Some(429), json!({}), ErrorKind::RateLimited),
            (Some(400), json!({}), ErrorKind::InvalidRequest),
            (Some(413), json!({}), ErrorKind::InvalidRequest),
            (Some(503), json!({}), ErrorKind::Unavailable),
            // dsh's `HTTP_404`, which it doesn't retry.
            (Some(404), json!({}), ErrorKind::InvalidRequest),
            (
                None,
                json!({"type": "authentication_error"}),
                ErrorKind::Auth,
            ),
            (
                None,
                json!({"type": "rate_limit_error"}),
                ErrorKind::RateLimited,
            ),
            (
                None,
                json!({"type": "invalid_request_error"}),
                ErrorKind::InvalidRequest,
            ),
            (None, json!({}), ErrorKind::Unavailable),
            (
                Some(400),
                json!({"message": "maximum context length exceeded"}),
                ErrorKind::ContextWindowExceeded,
            ),
            (
                Some(400),
                json!({"message": "insufficient balance"}),
                ErrorKind::Quota,
            ),
            (
                Some(400),
                json!({"code": "insufficient_balance"}),
                ErrorKind::Quota,
            ),
        ];
        for (status, error, expected) in cases {
            assert_eq!(
                classify(error.clone(), status),
                expected,
                "{status:?} {error}"
            );
        }
    }

    #[test]
    fn an_error_with_no_message_gets_dshs_wording() {
        let error = WireError::default().into_model_error(Some(500));
        assert_eq!(error.message, "DeepSeek's request failed (HTTP 500)");
        let error = WireError::default().into_model_error(None);
        assert_eq!(error.message, "DeepSeek's request failed (stream error)");
    }

    #[test]
    fn parts_that_arent_strings_count_as_missing() {
        let error: WireError =
            serde_json::from_value(json!({"type": 7, "code": null, "message": ["x"]}))
                .expect("the error reads");
        let error = error.into_model_error(Some(503));
        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert_eq!(error.message, "DeepSeek's request failed (HTTP 503)");
    }

    #[test]
    fn any_event_reads_as_an_error_event_but_only_errors_say_so() {
        let ping: ErrorEvent = serde_json::from_str(r#"{"type":"ping"}"#).expect("it reads");
        assert_eq!(ping.kind, "ping");
        let error: ErrorEvent =
            serde_json::from_str(r#"{"type":"error","error":{"type":"overloaded_error"}}"#)
                .expect("it reads");
        assert_eq!(error.kind, "error");
        assert_eq!(
            error.error.into_model_error(None).kind,
            ErrorKind::Unavailable
        );
    }
}
