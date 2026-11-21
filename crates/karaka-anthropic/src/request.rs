//! A [`Request`] as the JSON body Anthropic's Messages API expects.
//!
//! The wire types borrow from the request (`&'a str`) instead of copying it:
//! the body exists only long enough to be written out, so there's nothing to
//! own.
//!
//! Besides the translation itself, this module does three things a
//! conversation from another provider needs before Anthropic accepts it:
//!
//! - It drops text that is empty or only whitespace, which Anthropic refuses
//!   with a 400.
//! - It turns reasoning into text unless this model signed it.
//! - It leaves out a message with nothing left in it.
//!
//! Switching providers in the middle of a session is normal, so these come
//! up in ordinary use, not only in broken histories.
//!
//! # System and developer messages
//!
//! Newer models take `{"role": "system"}` messages inside `messages`. For
//! them, each system or developer message in the history goes out as one,
//! as pi-ai sends them (`api/anthropic-messages.js:1007-1043`). For the
//! other models pi-ai folds them into the system prompt instead
//! (`utils/transcript.js:96-104`), and so does this module. See
//! [`takes_system_messages`].

use std::borrow::Cow;

use karaka_model::{Block, Effort, ErrorKind, Message, ModelError, Replay, Request, Role};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// pi-ai's name for this wire format. It goes in the [`Origin`] of every
/// signature this profile receives, beside [`PROVIDER`] and the model.
///
/// [`Origin`]: karaka_model::Origin
pub(crate) const API: &str = "anthropic-messages";

/// pi-ai's name for Anthropic itself, as its catalog has it.
pub(crate) const PROVIDER: &str = "anthropic";

/// The beta that allows `thinking.block_binding`. Without this header
/// Anthropic rejects the field with "Extra inputs are not permitted".
pub(crate) const THINKING_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

/// The settings that come from the provider's config rather than from each
/// request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings {
    /// For a request that doesn't set `max_tokens`.
    pub default_max_tokens: u32,
    /// For a request that doesn't set `effort`.
    pub default_effort: Option<Effort>,
    pub thinking_display: Option<ThinkingDisplay>,
    pub cache: CacheMode,
}

/// What Anthropic sends back of the model's thinking.
///
/// It decides only what is shown. Current models think either way, and the
/// thinking is billed the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingDisplay {
    /// A readable summary of the reasoning.
    Summarized,
    /// Thinking blocks with empty text. This is Anthropic's own default on
    /// current models.
    Omitted,
}

/// How long Anthropic keeps the start of a conversation cached, so the next
/// request reads it for a tenth of the price or less.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    /// Five minutes, renewed by every request that reads the cache. Writing
    /// costs 1.25 times the normal input price.
    #[default]
    Short,
    /// One hour. Writing costs twice the normal input price, so it pays off
    /// only when requests that share a start come 5 to 60 minutes apart.
    Long,
    /// Nothing is cached.
    Off,
}

impl CacheMode {
    /// The marker to put on each cached block, or `None` for no caching.
    fn marker(self) -> Option<CacheControl> {
        let ttl = match self {
            Self::Short => None,
            Self::Long => Some("1h"),
            Self::Off => return None,
        };
        Some(CacheControl {
            kind: "ephemeral",
            ttl,
        })
    }
}

/// The body of `POST /v1/messages`.
#[derive(Serialize)]
pub(crate) struct Body<'a> {
    model: &'a str,
    /// Anthropic requires it, so a request that leaves it to the provider
    /// gets the provider's configured default.
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<System<'a>>,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Thinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
    stream: bool,
}

impl Body<'_> {
    /// The `anthropic-beta` header this body needs, if any.
    pub(crate) fn beta(&self) -> Option<&'static str> {
        self.thinking.as_ref().map(|_| THINKING_BINDING_BETA)
    }
}

/// The system prompt, which Anthropic takes in two forms. `untagged` writes
/// out whichever variant it is without naming it.
#[derive(Serialize)]
#[serde(untagged)]
enum System<'a> {
    /// A plain string, when there's nothing to add to it.
    Text(Cow<'a, str>),
    /// A list of text blocks, here always one. Only a block can carry a
    /// cache marker.
    Blocks([SystemBlock<'a>; 1]),
}

#[derive(Serialize)]
struct SystemBlock<'a> {
    /// Always `"text"`.
    #[serde(rename = "type")]
    kind: &'static str,
    text: Cow<'a, str>,
    cache_control: CacheControl,
}

/// A cache breakpoint. Anthropic caches everything up to and including the
/// block that carries it, in the order tools, system prompt, messages.
#[derive(Clone, Copy, Serialize)]
struct CacheControl {
    /// Always `"ephemeral"`, the only kind there is.
    #[serde(rename = "type")]
    kind: &'static str,
    /// Leaving it out means five minutes.
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<&'static str>,
}

/// Adaptive thinking: the model decides when to think and for how long,
/// guided by the effort.
#[derive(Serialize)]
struct Thinking {
    /// Always `"adaptive"`.
    #[serde(rename = "type")]
    kind: &'static str,
    display: &'static str,
    block_binding: BlockBinding,
}

/// What Anthropic does with a thinking block whose history has changed.
///
/// A thinking block's signature records the system prompt, the tools, and
/// every message before it. If any of those changed since the block was
/// written, for example because the history was compacted, newer models
/// refuse the request. Retrying the same body fails the same way, so the
/// session would be stuck. `"drop_block"` makes Anthropic drop that thinking
/// and carry on instead.
#[derive(Serialize)]
struct BlockBinding {
    prefix_mismatch_behavior: &'static str,
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

/// One content block. `tag = "type"` writes the variant's name into the
/// object as `"type": "tool_use"` and so on, which is how Anthropic tells
/// blocks apart.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireBlock<'a> {
    Text {
        /// Owned only for the text of a developer message, whose text
        /// blocks are joined.
        text: Cow<'a, str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Thinking can't carry a cache marker, so it has no field for one.
    Thinking {
        thinking: &'a str,
        signature: &'a str,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        /// The model's own JSON, written out as it was.
        input: &'a RawValue,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
        #[serde(skip_serializing_if = "is_false")]
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

impl<'a> WireBlock<'a> {
    fn text(text: impl Into<Cow<'a, str>>) -> Self {
        Self::Text {
            text: text.into(),
            cache_control: None,
        }
    }

    /// Puts `marker` on this block, and says whether it could.
    fn mark(&mut self, marker: CacheControl) -> bool {
        match self {
            Self::Text { cache_control, .. }
            | Self::ToolUse { cache_control, .. }
            | Self::ToolResult { cache_control, .. } => {
                *cache_control = Some(marker);
                true
            }
            Self::Thinking { .. } => false,
        }
    }
}

#[derive(Serialize)]
struct WireTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a RawValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// Builds the streaming body for `request`.
///
/// Fails only if a tool's parameters aren't a JSON object, which is a bug in
/// whoever defined the tool, so it's reported before anything is sent.
pub(crate) fn body<'a>(request: &'a Request, settings: &Settings) -> Result<Body<'a>, ModelError> {
    let mut tools: Vec<WireTool<'_>> = request
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
                cache_control: None,
            })
        })
        .collect::<Result<_, ModelError>>()?;
    let (mut messages, folded) = messages(request);

    // Three breakpoints at most, so the limit of four is never reached. The
    // markers all keep the cache for the same time, which matters because
    // Anthropic wants markers that keep it longer to come first.
    let marker = settings.cache.marker();
    if let Some(marker) = marker {
        // The tools come first in what Anthropic caches, and change least.
        if let Some(tool) = tools.last_mut() {
            tool.cache_control = Some(marker);
        }
        // The whole conversation so far, so the next request reads it all
        // back. In a tool loop the last block is a tool result, and that
        // takes a marker too. A system message at the end takes it, as in
        // pi-ai (`anthropic-messages.js:1175-1178`), so the cache reaches
        // past it.
        if let Some(last) = messages.iter_mut().rev().find(|m| m.role != "assistant") {
            // The last block that can carry a marker.
            for block in last.content.iter_mut().rev() {
                if block.mark(marker) {
                    break;
                }
            }
        }
    }
    // A blank system prompt counts as none: Anthropic refuses an empty
    // text block, and a marker on it would cache nothing.
    let system = system_prompt(request.system.as_deref(), folded)
        .filter(|text| !text.trim().is_empty())
        .map(|text| match marker {
            // Caches the tools and the system prompt together.
            Some(cache_control) => System::Blocks([SystemBlock {
                kind: "text",
                text,
                cache_control,
            }]),
            None => System::Text(text),
        });

    // With no thinking setting, no `thinking` field is sent at all, and each
    // model does what it does by default. That keeps older models such as
    // Haiku 4.5 working, which reject adaptive thinking, at the cost of
    // getting each model's default display. On current models that is
    // "omitted", so their reasoning arrives empty.
    let thinking = settings.thinking_display.map(|display| Thinking {
        kind: "adaptive",
        display: match display {
            ThinkingDisplay::Summarized => "summarized",
            ThinkingDisplay::Omitted => "omitted",
        },
        block_binding: BlockBinding {
            prefix_mismatch_behavior: "drop_block",
        },
    });
    // Changing the effort between requests invalidates the cached messages,
    // so it is best set once per kind of work, not per request.
    let output_config = request
        .effort
        .or(settings.default_effort)
        .map(|effort| OutputConfig {
            effort: effort_name(effort),
        });

    Ok(Body {
        model: &request.model,
        max_tokens: request.max_tokens.unwrap_or(settings.default_max_tokens),
        system,
        messages,
        tools,
        thinking,
        output_config,
        stream: true,
    })
}

/// Anthropic's name for each effort level. Current models take all five.
///
/// | Effort   | `output_config.effort` |
/// |----------|------------------------|
/// | `Low`    | `"low"`                |
/// | `Medium` | `"medium"`             |
/// | `High`   | `"high"`, the default  |
/// | `XHigh`  | `"xhigh"`              |
/// | `Max`    | `"max"`                |
///
/// Older models may lack `xhigh`, and Haiku 4.5 takes no effort at all.
/// Those are left to refuse it, since guessing a model's age from its name
/// would go wrong as new models arrive.
fn effort_name(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

/// Whether `model` takes `{"role": "system"}` messages in the middle of
/// the conversation.
///
/// These are the models pi-ai 1.1.0's catalog marks with
/// `supportsMidConvoSystemMessages` (`providers/data/anthropic.json`), and
/// the ones Anthropic's prompt caching docs add: Fable 5.2 and the Mythos
/// models. The docs leave out Sonnet 5, and so does pi-ai.
///
/// The list names exact model ids rather than guessing from a name's shape.
/// A model missing from it still works: its instructions are folded into
/// the system prompt, as pi-ai does for every model it doesn't mark. They
/// then lose their place, and changing one stops the cache at the start of
/// the conversation. A model wrongly on the list would fail every request
/// that has one, with a 400.
fn takes_system_messages(model: &str) -> bool {
    matches!(
        model,
        "claude-fable-5"
            | "claude-fable-5-1"
            | "claude-fable-5-2"
            | "claude-haiku-5-5"
            | "claude-mythos-5"
            | "claude-mythos-5-1"
            | "claude-mythos-5-2"
            | "claude-opus-4-8"
            | "claude-opus-5"
            | "claude-opus-5-5"
            | "claude-sonnet-5-5"
    )
}

/// The messages on the wire, and the text of the system and developer
/// messages that were folded into the system prompt instead.
///
/// Where the model takes system messages, each one waits for the next
/// assistant message and goes out just before it, or at the end. That is
/// where pi-ai puts them (`anthropic-messages.js:1007-1018,1088,1174`): Anthropic
/// wants a tool's results right after its call, so nothing may come between
/// them, and it wants a system message to follow a user turn.
fn messages(request: &Request) -> (Vec<WireMessage<'_>>, Vec<Cow<'_, str>>) {
    let in_history = takes_system_messages(&request.model);
    let mut wire = Vec::new();
    let mut folded = Vec::new();
    let mut waiting = Vec::new();
    for message in &request.messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => {
                // pi-ai lets them out even when the assistant message
                // itself turns out empty and is skipped.
                wire.append(&mut waiting);
                "assistant"
            }
            Role::System | Role::Developer => {
                let text = instruction_text(message);
                // pi-ai skips only empty text. Anthropic refuses text that
                // is only whitespace, so that is skipped too.
                if text.trim().is_empty() {
                    continue;
                }
                if in_history {
                    waiting.push(WireMessage {
                        role: "system",
                        content: vec![WireBlock::text(text)],
                    });
                } else {
                    folded.push(text);
                }
                continue;
            }
        };
        let content: Vec<_> = message
            .content
            .iter()
            .filter_map(|block| wire_block(block, &request.model))
            .collect();
        // Anthropic refuses a message with no content. Leaving one out can
        // put two messages with the same role next to each other, which
        // Anthropic accepts and reads as one turn.
        if !content.is_empty() {
            wire.push(WireMessage { role, content });
        }
    }
    wire.append(&mut waiting);
    (wire, folded)
}

/// The text of a system or developer message: its text blocks joined with
/// a newline, as pi-ai's `contentText` joins them (`utils/text.js:2-9`).
/// Derivation gives a system message one block, and a developer message one
/// per piece of text.
fn instruction_text(message: &Message) -> Cow<'_, str> {
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
        _ => Cow::Owned(texts.join("\n")),
    }
}

/// The system prompt: the request's own, then each folded instruction, the
/// non-empty ones joined with a blank line. pi-ai builds the folded prompt
/// the same way (`utils/transcript.js:58-86`).
fn system_prompt<'a>(system: Option<&'a str>, folded: Vec<Cow<'a, str>>) -> Option<Cow<'a, str>> {
    if folded.is_empty() {
        return system.map(Cow::Borrowed);
    }
    let parts: Vec<Cow<'a, str>> = system
        .map(Cow::Borrowed)
        .into_iter()
        .chain(folded)
        .filter(|part| !part.is_empty())
        .collect();
    Some(Cow::Owned(parts.join("\n\n")))
}

/// Whether this profile can send `replay`'s signature with a request to
/// `model`: Anthropic wrote it, through this API, for this same model.
/// pi-ai checks the same three things (`transform-messages.js:68-70`).
fn is_own(replay: &Replay, model: &str) -> bool {
    replay.origin.api == API && replay.origin.provider == PROVIDER && replay.origin.model == model
}

/// One block on the wire for a request to `model`, or `None` for a block
/// with nothing to send.
fn wire_block<'a>(block: &'a Block, model: &str) -> Option<WireBlock<'a>> {
    Some(match block {
        // Anthropic refuses text that is empty or only whitespace.
        Block::Text(text) if text.trim().is_empty() => return None,
        Block::Text(text) => WireBlock::text(text.as_str()),
        // Anthropic accepts thinking back only with the signature it issued,
        // to the model it issued it for. A signed block is kept even when
        // its text is empty: with display "omitted" every block is, and the
        // signature still carries the reasoning.
        Block::Reasoning {
            text,
            replay: Some(replay),
        } if is_own(replay, model) && !replay.signature.trim().is_empty() => WireBlock::Thinking {
            thinking: text,
            signature: &replay.signature,
        },
        // Any other reasoning came from another model or provider, or from
        // a stream that broke off. Anthropic won't take it as thinking, but
        // as text it keeps what the model had worked out. pi-ai does the
        // same (`transform-messages.js:82-90`, `anthropic-messages.js:1112-1124`).
        Block::Reasoning { text, .. } if text.trim().is_empty() => return None,
        Block::Reasoning { text, .. } => WireBlock::text(text.as_str()),
        Block::ToolCall(call) => WireBlock::ToolUse {
            id: &call.id,
            name: &call.name,
            // Broken arguments still have to go back as an object. The tool
            // already told the model they were broken, in its result.
            // A closure, not `empty_object` itself: passing the function would make
            // the whole `Option` `'static`, and the arguments are only borrowed.
            input: json_object(&call.arguments).unwrap_or_else(|| empty_object()),
            cache_control: None,
        },
        Block::ToolResult(result) => WireBlock::ToolResult {
            tool_use_id: &result.call_id,
            content: &result.content,
            is_error: result.is_error,
            cache_control: None,
        },
    })
}

/// `text` as raw JSON, if it is a JSON object. Checked, but not parsed into
/// values: it goes out exactly as it came in.
pub(crate) fn json_object(text: &str) -> Option<&RawValue> {
    let raw: &RawValue = serde_json::from_str(text).ok()?;
    raw.get().starts_with('{').then_some(raw)
}

pub(crate) fn empty_object() -> &'static RawValue {
    serde_json::from_str("{}").expect("`{}` is valid JSON")
}

#[cfg(test)]
mod tests {
    use karaka_model::{
        Block, Effort, ErrorKind, Message, Origin, Replay, Request, Role, ToolCall, ToolResult,
        ToolSchema,
    };
    use serde_json::{Value, json};

    use super::{CacheMode, Settings, THINKING_BINDING_BETA, ThinkingDisplay, body};

    /// The model most tests ask for. It takes system messages in place.
    const MODEL: &str = "claude-opus-5-5";

    /// A model that doesn't take system messages in place.
    const OLDER: &str = "claude-sonnet-4-6";

    /// No caching, no thinking and no effort: the body with nothing added,
    /// so a test of how messages translate shows only that.
    fn plain() -> Settings {
        Settings {
            default_max_tokens: 1000,
            default_effort: None,
            thinking_display: None,
            cache: CacheMode::Off,
        }
    }

    fn cached(cache: CacheMode) -> Settings {
        Settings { cache, ..plain() }
    }

    fn json_with(request: &Request, settings: &Settings) -> Value {
        serde_json::to_value(body(request, settings).expect("the body builds"))
            .expect("it serializes")
    }

    fn to_json(request: &Request) -> Value {
        json_with(request, &plain())
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

    /// Reasoning that `MODEL` signed with `signature`, or no one did.
    fn reasoning(text: &str, signature: Option<&str>) -> Block {
        Block::Reasoning {
            text: text.into(),
            replay: signature.map(|signature| signed_by("anthropic", MODEL, signature)),
        }
    }

    /// A signature from `model` at `provider`, through Anthropic's API, or
    /// through DeepSeek's when the provider is `deepseek`.
    fn signed_by(provider: &str, model: &str, signature: &str) -> Replay {
        let api = match provider {
            "deepseek" => "deepseek-messages",
            _ => "anthropic-messages",
        };
        Replay::new(Origin::new(api, provider, model), signature)
    }

    fn reasoning_from(text: &str, replay: Replay) -> Block {
        Block::Reasoning {
            text: text.into(),
            replay: Some(replay),
        }
    }

    fn developer(texts: &[&str]) -> Message {
        Message {
            role: Role::Developer,
            content: texts.iter().map(|t| text(t)).collect(),
        }
    }

    fn clock_call() -> Block {
        Block::ToolCall(ToolCall {
            id: "toolu_1".into(),
            name: "clock".into(),
            arguments: r#"{"zone": "UTC"}"#.into(),
        })
    }

    fn clock_result() -> Block {
        Block::ToolResult(ToolResult {
            call_id: "toolu_1".into(),
            content: "12:00".into(),
            is_error: false,
        })
    }

    fn clock_tool() -> ToolSchema {
        ToolSchema {
            name: "clock".into(),
            description: "Tells the time.".into(),
            parameters: r#"{"type": "object"}"#.into(),
        }
    }

    fn calendar_tool() -> ToolSchema {
        ToolSchema {
            name: "calendar".into(),
            description: "Tells the date.".into(),
            parameters: r#"{"type": "object"}"#.into(),
        }
    }

    /// A system prompt, two tools, and one round of a tool call: the
    /// conversation ends in the tool's result, as it does inside a tool loop.
    fn tool_round() -> Request {
        let mut request = Request::new(
            "claude-opus-5-5",
            vec![
                Message::user("what time is it?"),
                assistant(vec![
                    reasoning("check the clock", Some("sig")),
                    clock_call(),
                ]),
                user(vec![clock_result()]),
            ],
        );
        request.system = Some("Be brief.".into());
        request.tools = vec![calendar_tool(), clock_tool()];
        request
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
                assistant(vec![
                    reasoning("check the clock", Some("sig")),
                    reasoning("from another provider", None),
                    clock_call(),
                ]),
                user(vec![clock_result()]),
            ],
        );
        request.max_tokens = Some(64);
        request.tools.push(clock_tool());

        assert_eq!(
            to_json(&request),
            json!({
                "model": "claude-opus-5-5",
                "max_tokens": 64,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "what time is it?"}]},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "check the clock", "signature": "sig"},
                        {"type": "text", "text": "from another provider"},
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
            vec![assistant(vec![Block::ToolCall(ToolCall {
                id: "toolu_1".into(),
                name: "clock".into(),
                arguments: r#"{"zone": "#.into(),
            })])],
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
        let Err(error) = body(&request, &plain()) else {
            panic!("expected the body to fail");
        };
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert!(error.message.contains("\"clock\""), "{error}");
    }

    // Effort.

    #[test]
    fn the_requests_effort_goes_out_as_output_config() {
        let mut request = Request::new("claude-opus-5-5", vec![Message::user("hi")]);
        request.effort = Some(Effort::XHigh);

        assert_eq!(
            to_json(&request),
            json!({
                "model": "claude-opus-5-5",
                "max_tokens": 1000,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "hi"}]}
                ],
                "output_config": {"effort": "xhigh"},
                "stream": true
            })
        );
    }

    #[test]
    fn the_configs_effort_applies_when_the_request_sets_none() {
        let settings = Settings {
            default_effort: Some(Effort::Medium),
            ..plain()
        };
        let mut request = Request::new("claude-opus-5-5", vec![Message::user("hi")]);
        assert_eq!(
            json_with(&request, &settings)["output_config"],
            json!({"effort": "medium"})
        );

        request.effort = Some(Effort::Low);
        assert_eq!(
            json_with(&request, &settings)["output_config"],
            json!({"effort": "low"})
        );
    }

    #[test]
    fn with_no_effort_anywhere_there_is_no_output_config() {
        let request = Request::new("claude-opus-5-5", vec![Message::user("hi")]);
        assert!(to_json(&request).get("output_config").is_none());
    }

    #[test]
    fn every_effort_level_keeps_its_name() {
        let levels = [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::XHigh, "xhigh"),
            (Effort::Max, "max"),
        ];
        for (effort, name) in levels {
            let mut request = Request::new("claude-opus-5-5", vec![Message::user("hi")]);
            request.effort = Some(effort);
            assert_eq!(to_json(&request)["output_config"]["effort"], name);
        }
    }

    // Thinking.

    #[test]
    fn a_thinking_display_turns_on_adaptive_thinking() {
        let settings = Settings {
            thinking_display: Some(ThinkingDisplay::Summarized),
            ..plain()
        };
        let request = Request::new("claude-opus-5-5", vec![Message::user("hi")]);
        let built = body(&request, &settings).expect("the body builds");
        assert_eq!(built.beta(), Some(THINKING_BINDING_BETA));

        assert_eq!(
            serde_json::to_value(built).expect("it serializes"),
            json!({
                "model": "claude-opus-5-5",
                "max_tokens": 1000,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "hi"}]}
                ],
                "thinking": {
                    "type": "adaptive",
                    "display": "summarized",
                    "block_binding": {"prefix_mismatch_behavior": "drop_block"}
                },
                "stream": true
            })
        );
    }

    #[test]
    fn omitted_display_is_sent_as_omitted() {
        let settings = Settings {
            thinking_display: Some(ThinkingDisplay::Omitted),
            ..plain()
        };
        let request = Request::new("claude-opus-5-5", vec![Message::user("hi")]);
        assert_eq!(
            json_with(&request, &settings)["thinking"]["display"],
            "omitted"
        );
    }

    #[test]
    fn with_no_thinking_display_there_is_no_thinking_field_and_no_beta() {
        let request = Request::new("claude-opus-5-5", vec![Message::user("hi")]);
        let built = body(&request, &plain()).expect("the body builds");
        assert_eq!(built.beta(), None);
        assert!(
            serde_json::to_value(built)
                .expect("it serializes")
                .get("thinking")
                .is_none()
        );
    }

    // Caching.

    #[test]
    fn short_caching_marks_the_tools_the_system_prompt_and_the_last_result() {
        assert_eq!(
            json_with(&tool_round(), &cached(CacheMode::Short)),
            json!({
                "model": "claude-opus-5-5",
                "max_tokens": 1000,
                "system": [
                    {"type": "text", "text": "Be brief.", "cache_control": {"type": "ephemeral"}}
                ],
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "what time is it?"}]},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "check the clock", "signature": "sig"},
                        {"type": "tool_use", "id": "toolu_1", "name": "clock", "input": {"zone": "UTC"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "12:00",
                         "cache_control": {"type": "ephemeral"}}
                    ]}
                ],
                "tools": [
                    {"name": "calendar", "description": "Tells the date.", "input_schema": {"type": "object"}},
                    {"name": "clock", "description": "Tells the time.", "input_schema": {"type": "object"},
                     "cache_control": {"type": "ephemeral"}}
                ],
                "stream": true
            })
        );
    }

    #[test]
    fn long_caching_asks_for_an_hour_on_every_marker() {
        let hour = json!({"type": "ephemeral", "ttl": "1h"});
        assert_eq!(
            json_with(&tool_round(), &cached(CacheMode::Long)),
            json!({
                "model": "claude-opus-5-5",
                "max_tokens": 1000,
                "system": [{"type": "text", "text": "Be brief.", "cache_control": hour}],
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "what time is it?"}]},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "check the clock", "signature": "sig"},
                        {"type": "tool_use", "id": "toolu_1", "name": "clock", "input": {"zone": "UTC"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "12:00",
                         "cache_control": hour}
                    ]}
                ],
                "tools": [
                    {"name": "calendar", "description": "Tells the date.", "input_schema": {"type": "object"}},
                    {"name": "clock", "description": "Tells the time.", "input_schema": {"type": "object"},
                     "cache_control": hour}
                ],
                "stream": true
            })
        );
    }

    #[test]
    fn caching_off_sends_no_markers_and_a_plain_system_prompt() {
        assert_eq!(
            json_with(&tool_round(), &cached(CacheMode::Off)),
            json!({
                "model": "claude-opus-5-5",
                "max_tokens": 1000,
                "system": "Be brief.",
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
                    {"name": "calendar", "description": "Tells the date.", "input_schema": {"type": "object"}},
                    {"name": "clock", "description": "Tells the time.", "input_schema": {"type": "object"}}
                ],
                "stream": true
            })
        );
    }

    #[test]
    fn short_caching_is_the_default() {
        assert_eq!(CacheMode::default(), CacheMode::Short);
    }

    #[test]
    fn a_long_system_prompt_is_one_marked_block() {
        let prompt = "Answer in one sentence. ".repeat(500);
        let mut request = Request::new("claude-opus-5-5", vec![Message::user("hi")]);
        request.system = Some(prompt.clone());

        assert_eq!(
            json_with(&request, &cached(CacheMode::Short)),
            json!({
                "model": "claude-opus-5-5",
                "max_tokens": 1000,
                "system": [
                    {"type": "text", "text": prompt, "cache_control": {"type": "ephemeral"}}
                ],
                "messages": [
                    {"role": "user", "content": [
                        {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
                    ]}
                ],
                "stream": true
            })
        );
    }

    #[test]
    fn with_no_system_prompt_only_the_tools_and_the_messages_are_marked() {
        let mut request = tool_round();
        request.system = None;

        let sent = json_with(&request, &cached(CacheMode::Short));
        assert!(sent.get("system").is_none(), "{sent}");
        assert_eq!(markers(&sent), 2, "{sent}");
        assert_eq!(
            sent["tools"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(
            sent["messages"][2]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn a_blank_system_prompt_is_left_out() {
        let mut request = Request::new("claude-opus-5-5", vec![Message::user("hi")]);
        request.system = Some(" \n".into());
        for cache in [CacheMode::Short, CacheMode::Off] {
            assert!(json_with(&request, &cached(cache)).get("system").is_none());
        }
    }

    #[test]
    fn no_request_carries_more_than_four_markers() {
        for cache in [CacheMode::Short, CacheMode::Long] {
            assert_eq!(markers(&json_with(&tool_round(), &cached(cache))), 3);
        }
        assert_eq!(
            markers(&json_with(&tool_round(), &cached(CacheMode::Off))),
            0
        );
    }

    #[test]
    fn only_the_last_user_message_is_marked() {
        let request = Request::new(
            "claude-opus-5-5",
            vec![
                Message::user("first"),
                assistant(vec![text("ok")]),
                user(vec![text("second"), text("third")]),
            ],
        );
        let sent = json_with(&request, &cached(CacheMode::Short));
        assert_eq!(markers(&sent), 1, "{sent}");
        assert_eq!(
            sent["messages"][2]["content"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    // Empty blocks.

    #[test]
    fn empty_and_whitespace_text_is_dropped() {
        let request = Request::new(
            "claude-opus-5-5",
            vec![
                user(vec![text(""), text("hi"), text("  \n\t")]),
                assistant(vec![text(" "), text("hello")]),
            ],
        );
        assert_eq!(
            to_json(&request)["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "hello"}]}
            ])
        );
    }

    #[test]
    fn unsigned_reasoning_becomes_text() {
        let request = Request::new(
            "claude-opus-5-5",
            vec![assistant(vec![
                reasoning("DeepSeek thought this", None),
                reasoning("an empty signature is no signature", Some("")),
                text("answer"),
            ])],
        );
        assert_eq!(
            to_json(&request)["messages"][0]["content"],
            json!([
                {"type": "text", "text": "DeepSeek thought this"},
                {"type": "text", "text": "an empty signature is no signature"},
                {"type": "text", "text": "answer"}
            ])
        );
    }

    #[test]
    fn blank_unsigned_reasoning_is_dropped_but_signed_reasoning_is_kept() {
        let request = Request::new(
            "claude-opus-5-5",
            vec![assistant(vec![
                reasoning("  ", None),
                // What display "omitted" returns: no text, but a signature.
                reasoning("", Some("sig")),
                text("answer"),
            ])],
        );
        assert_eq!(
            to_json(&request)["messages"][0]["content"],
            json!([
                {"type": "thinking", "thinking": "", "signature": "sig"},
                {"type": "text", "text": "answer"}
            ])
        );
    }

    #[test]
    fn a_message_left_empty_is_skipped() {
        let request = Request::new(
            "claude-opus-5-5",
            vec![
                Message::user("hi"),
                assistant(vec![text(""), reasoning(" ", None)]),
                Message::user("still there?"),
            ],
        );
        assert_eq!(
            to_json(&request)["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "user", "content": [{"type": "text", "text": "still there?"}]}
            ])
        );
    }

    #[test]
    fn the_marker_lands_on_the_last_block_left_after_filtering() {
        let request = Request::new(
            "claude-opus-5-5",
            vec![user(vec![clock_result(), text("thanks"), text("   ")])],
        );
        assert_eq!(
            json_with(&request, &cached(CacheMode::Short))["messages"][0]["content"],
            json!([
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "12:00"},
                {"type": "text", "text": "thanks", "cache_control": {"type": "ephemeral"}}
            ])
        );
    }

    #[test]
    fn the_marker_skips_a_user_message_that_was_left_out() {
        let request = Request::new(
            "claude-opus-5-5",
            vec![
                Message::user("hi"),
                assistant(vec![text("hello")]),
                user(vec![text(" ")]),
            ],
        );
        let sent = json_with(&request, &cached(CacheMode::Short));
        assert_eq!(sent["messages"].as_array().map(Vec::len), Some(2), "{sent}");
        assert_eq!(
            sent["messages"][0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    /// Reasoning that DeepSeek signed goes to Anthropic as text, as any
    /// other provider's does: Anthropic would refuse DeepSeek's signature.
    #[test]
    fn deepseek_reasoning_becomes_text() {
        let request = Request::new(
            "claude-opus-5-5",
            vec![assistant(vec![
                reasoning_from(
                    "DeepSeek thought this",
                    signed_by("deepseek", "deepseek-flash", "sig"),
                ),
                text("answer"),
            ])],
        );
        assert_eq!(
            to_json(&request)["messages"][0]["content"],
            json!([
                {"type": "text", "text": "DeepSeek thought this"},
                {"type": "text", "text": "answer"}
            ])
        );
    }

    // Signatures from elsewhere. The cases follow dsh's pi-ai tests,
    // "recombines durable content with pi-ai replay metadata across target
    // providers and models" (`llm-pi-ai/tests/convert.spec.ts:359`), and
    // what pi-ai then does with each (`transform-messages.js:66-91`).

    /// Only the model that signed gets the signature. Any other model, at
    /// Anthropic or elsewhere, gets the reasoning as text, and loses
    /// reasoning that has no text at all.
    #[test]
    fn a_signature_goes_back_only_to_the_model_that_signed_it() {
        let history = |replay: Replay| {
            vec![
                Message::user("hi"),
                assistant(vec![
                    reasoning_from("thinking", replay.clone()),
                    reasoning_from("", replay),
                    text("answer"),
                ]),
            ]
        };
        let signed = signed_by("anthropic", MODEL, "sig");
        assert_eq!(
            to_json(&Request::new(MODEL, history(signed.clone())))["messages"][1]["content"],
            json!([
                {"type": "thinking", "thinking": "thinking", "signature": "sig"},
                {"type": "thinking", "thinking": "", "signature": "sig"},
                {"type": "text", "text": "answer"}
            ])
        );

        let degraded = json!([
            {"type": "text", "text": "thinking"},
            {"type": "text", "text": "answer"}
        ]);
        // Another Anthropic model.
        assert_eq!(
            to_json(&Request::new("claude-sonnet-5-5", history(signed)))["messages"][1]["content"],
            degraded
        );
        // The same model's name, but another provider or another API.
        for replay in [
            signed_by("openrouter", MODEL, "sig"),
            Replay::new(Origin::new("openai-responses", "anthropic", MODEL), "sig"),
            signed_by("deepseek", MODEL, "sig"),
        ] {
            assert_eq!(
                to_json(&Request::new(MODEL, history(replay)))["messages"][1]["content"],
                degraded
            );
        }
    }

    /// Anthropic to DeepSeek and back: Anthropic's own signature is still
    /// there for it, and DeepSeek's reasoning in between is text.
    #[test]
    fn a_signature_survives_a_trip_through_deepseek() {
        let request = Request::new(
            MODEL,
            vec![
                Message::user("hi"),
                assistant(vec![reasoning("Claude thought", Some("claude-sig"))]),
                Message::user("again"),
                assistant(vec![reasoning_from(
                    "DeepSeek thought",
                    signed_by("deepseek", "deepseek-flash", "ds-sig"),
                )]),
                Message::user("once more"),
            ],
        );
        let sent = to_json(&request);
        assert_eq!(
            sent["messages"][1]["content"],
            json!([{"type": "thinking", "thinking": "Claude thought", "signature": "claude-sig"}])
        );
        assert_eq!(
            sent["messages"][3]["content"],
            json!([{"type": "text", "text": "DeepSeek thought"}])
        );
    }

    // System and developer messages.

    /// The test the brief asks for: system "A", user, assistant, system
    /// "B", user. "A" stays the prompt. For a model that takes system
    /// messages, "B" waits for the next assistant message, so it goes out
    /// after the last user message, and that is where the cache marker
    /// goes.
    #[test]
    fn a_later_system_message_goes_in_place_where_the_model_takes_it() {
        let mut request = Request::new(
            MODEL,
            vec![
                Message::user("first"),
                assistant(vec![text("one")]),
                Message::system("B"),
                Message::user("second"),
            ],
        );
        request.system = Some("A".into());
        assert_eq!(
            json_with(&request, &cached(CacheMode::Short)),
            json!({
                "model": MODEL,
                "max_tokens": 1000,
                "system": [{"type": "text", "text": "A", "cache_control": {"type": "ephemeral"}}],
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "first"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "one"}]},
                    {"role": "user", "content": [{"type": "text", "text": "second"}]},
                    {"role": "system", "content": [
                        {"type": "text", "text": "B", "cache_control": {"type": "ephemeral"}}
                    ]}
                ],
                "stream": true
            })
        );
    }

    /// The same history for a model that doesn't take system messages:
    /// pi-ai folds "B" into the prompt after "A" (`utils/transcript.js:58-86`).
    #[test]
    fn a_later_system_message_is_folded_into_the_prompt_for_older_models() {
        let mut request = Request::new(
            OLDER,
            vec![
                Message::user("first"),
                assistant(vec![text("one")]),
                Message::system("B"),
                Message::user("second"),
            ],
        );
        request.system = Some("A".into());
        assert_eq!(
            to_json(&request),
            json!({
                "model": OLDER,
                "max_tokens": 1000,
                "system": "A\n\nB",
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "first"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "one"}]},
                    {"role": "user", "content": [{"type": "text", "text": "second"}]}
                ],
                "stream": true
            })
        );

        // With no prompt of its own, the folded text is the whole prompt.
        let request = Request::new(OLDER, vec![Message::user("hi"), Message::system("B")]);
        assert_eq!(to_json(&request)["system"], "B");
    }

    /// pi-ai holds a system message back until the next assistant message,
    /// because Anthropic wants tool results right after their call
    /// (`anthropic-messages.js:1007-1018`).
    #[test]
    fn a_system_message_waits_for_the_next_assistant_message() {
        let request = Request::new(
            MODEL,
            vec![
                Message::user("time?"),
                assistant(vec![clock_call()]),
                user(vec![clock_result()]),
                Message::system("Answer in French."),
                Message::user("and?"),
                assistant(vec![text("Midi.")]),
            ],
        );
        let roles: Vec<Value> = to_json(&request)["messages"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|message| message["role"].clone())
            .collect();
        assert_eq!(
            roles,
            ["user", "assistant", "user", "user", "system", "assistant"]
        );
    }

    /// Developer messages go the same way as system messages, their text
    /// blocks joined with a newline as pi-ai's `contentText` joins them.
    /// Blank ones are left out: Anthropic refuses blank text.
    #[test]
    fn developer_messages_go_in_place_or_into_the_prompt() {
        let history = vec![
            Message::user("hi"),
            developer(&["first", "second"]),
            developer(&[" "]),
            Message::system(""),
        ];
        let sent = to_json(&Request::new(MODEL, history.clone()));
        assert!(sent.get("system").is_none(), "{sent}");
        assert_eq!(
            sent["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "system", "content": [{"type": "text", "text": "first\nsecond"}]}
            ])
        );

        let mut request = Request::new(OLDER, history);
        request.system = Some("Be brief.".into());
        assert_eq!(to_json(&request)["system"], "Be brief.\n\nfirst\nsecond");
    }

    /// The models pi-ai's catalog marks, and the ones Anthropic's docs add.
    #[test]
    fn only_listed_models_take_system_messages() {
        for model in [
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5",
            "claude-fable-5-1",
            "claude-fable-5-2",
            "claude-mythos-5",
            "claude-mythos-5-1",
            "claude-mythos-5-2",
        ] {
            assert!(super::takes_system_messages(model), "{model}");
        }
        for model in [
            "claude-sonnet-5",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
            "claude-opus-4-7",
        ] {
            assert!(!super::takes_system_messages(model), "{model}");
        }
    }
}
