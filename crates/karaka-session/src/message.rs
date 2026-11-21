//! The messages dsh v4 logs inside its surface events.
//! (dsh v4: `packages/llm/llm/src/message.ts` and `types.ts`.)
//!
//! These mirror v4's JSON field for field, so a stored log reads and writes
//! back without loss. They are not Karaka's conversation types: those live in
//! `karaka-model`, and `convert.rs` maps between the two.
//!
//! # Fields Karaka doesn't know
//!
//! v4 lets producers add fields, block types and source kinds. Nothing here
//! drops them:
//!
//! - Each struct keeps every field it doesn't name in `extra`, and writes
//!   them back after its own fields.
//! - Each enum keeps a value whose tag it doesn't know whole, in `Other`.
//!
//! An optional field that holds any JSON, such as a tool result's `meta`,
//! tells a field that is absent from one that is `null`, since both are valid
//! v4 and they must not be confused when written back.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// The fields of a v4 object that Karaka doesn't name, kept as they were.
pub type Extra = Map<String, Value>;

/// Defines a type with one value, written as one fixed JSON string.
///
/// v4 messages carry tags that never change for a given event, such as a
/// tool result's `"role": "tool"`. Reading any other string is an error, as
/// in v4, which rejects a message whose role doesn't match its event.
macro_rules! fixed_tag {
    ($(#[$doc:meta])* $name:ident :: $variant:ident = $text:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name {
            #[default]
            #[serde(rename = $text)]
            $variant,
        }
    };
}

fixed_tag!(
    /// `"role": "system"`.
    SystemRole::System = "system"
);
fixed_tag!(
    /// `"role": "developer"`.
    DeveloperRole::Developer = "developer"
);
fixed_tag!(
    /// `"role": "user"`.
    UserRole::User = "user"
);
fixed_tag!(
    /// `"role": "assistant"`.
    AssistantRole::Assistant = "assistant"
);
fixed_tag!(
    /// `"role": "tool"`.
    ToolRole::Tool = "tool"
);
fixed_tag!(
    /// `"kind": "system-prompt"`, the source of every system message.
    SystemPromptKind::SystemPrompt = "system-prompt"
);
fixed_tag!(
    /// `"kind": "model"`, the source of every assistant message.
    ModelKind::Model = "model"
);
fixed_tag!(
    /// `"kind": "tool"`, the source of every tool result.
    ToolKind::Tool = "tool"
);

/// One piece of a message. (dsh v4: `ContentBlock`.)
///
/// Only the three blocks Karaka can send a model are typed. Images, files,
/// tool additions and removals, and any block a plugin adds stay in `Other`
/// as raw JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// dsh v4: `{ type: "text", text }`.
    #[serde(rename = "text")]
    Text(TextBlock),
    /// dsh v4: `{ type: "reasoning", text }`. The provider's signature isn't
    /// here: v4 keeps it in the assistant message's `source.replayState`.
    #[serde(rename = "reasoning")]
    Reasoning(ReasoningBlock),
    /// dsh v4: `{ type: "tool-call", id, name, arguments }`.
    #[serde(rename = "tool-call")]
    ToolCall(ToolCallBlock),
    /// Any other block, or a known block whose fields don't fit, as it was.
    #[serde(untagged)]
    Other(Value),
}

impl ContentBlock {
    /// A text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextBlock {
            text: text.into(),
            extra: Extra::new(),
        })
    }

    /// The block's `type` tag, such as `"text"` or `"image"`.
    pub fn type_name(&self) -> Option<&str> {
        match self {
            Self::Text(_) => Some("text"),
            Self::Reasoning(_) => Some("reasoning"),
            Self::ToolCall(_) => Some("tool-call"),
            Self::Other(value) => value.get("type").and_then(Value::as_str),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextBlock {
    pub text: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReasoningBlock {
    pub text: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallBlock {
    pub id: String,
    pub name: String,
    /// The arguments exactly as the model wrote them, unparsed.
    pub arguments: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// The rendered system prompt. (dsh v4: `SystemMessage`.)
///
/// Empty content means "no system prompt".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemMessage {
    /// Stable across every copy of the message. (dsh v4: `MessageId`.)
    pub id: String,
    pub role: SystemRole,
    pub content: Vec<ContentBlock>,
    pub source: SystemPromptSource,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh v4: `{ kind: "system-prompt" }`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemPromptSource {
    pub kind: SystemPromptKind,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A change to the agent's session, such as tools added or removed.
/// (dsh v4: `DeveloperMessage`.)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeveloperMessage {
    pub id: String,
    pub role: DeveloperRole,
    /// Text, and `tool-addition` and `tool-removal` blocks, which stay in
    /// [`ContentBlock::Other`].
    pub content: Vec<ContentBlock>,
    /// Whoever produced it, in v4's open vocabulary of sources.
    pub source: Value,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Something said in the user role. (dsh v4: `UserMessage`.)
///
/// A person's prompt and context the harness injects are both user
/// messages; `source` tells them apart.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    pub id: String,
    pub role: UserRole,
    pub content: Vec<ContentBlock>,
    pub source: UserSource,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Who produced a user message. (dsh v4: `MessageSource`, which plugins
/// extend with their own kinds.)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum UserSource {
    /// A person's prompt. (dsh v4: `{ kind: "user" }`, and the session
    /// controller's `user-rpc` form with `rpcId`.)
    #[serde(rename = "user")]
    User(PromptSource),
    /// Context the harness injects each step. (dsh v4: `runtime-context`.)
    #[serde(rename = "runtime-context")]
    RuntimeContext(RuntimeContextSource),
    /// Any other producer, such as a file watcher or an instructions file,
    /// as it was.
    #[serde(untagged)]
    Other(Value),
}

/// dsh v4: `{ kind: "user", rpcId?, clientTimeZone?, principal? }`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptSource {
    /// The id of the prompt request that sent this message. Play This Life
    /// uses it to make prompts idempotent and to scope write tools to the
    /// turn's own request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_time_zone: Option<String>,
    /// Who sent the prompt, when it wasn't the session's owner.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub principal: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh v4: `{ kind: "runtime-context" } & ContextFormed`.
///
/// `form` says what kind of context it is: `instructions`, `catalog`,
/// `snapshot`, `notice`, `relay` or `recall`. It stays a string, because v4
/// treats an unknown form as valid.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeContextSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<String>,
    /// A `snapshot`'s named parts, in the order they were assembled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<Vec<ContextSection>>,
    /// A `notice`'s one-line account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh v4: `ContextSnapshotSection`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextSection {
    pub name: String,
    pub text: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// One model call's answer. (dsh v4: `AssistantMessage`.)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub id: String,
    pub role: AssistantRole,
    pub content: Vec<ContentBlock>,
    pub source: ModelSource,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh v4: `ModelMessageSource`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSource {
    pub kind: ModelKind,
    /// The route that produced the message.
    pub provider: String,
    pub model: String,
    /// What the provider needs to accept this message back, such as
    /// reasoning signatures. Its `blocks` line up with `content` by index;
    /// see `convert.rs` for how it is read.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub replay_state: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// What one tool call returned. (dsh v4: `ToolResultMessage`.)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub id: String,
    pub role: ToolRole,
    pub source: ToolSource,
    /// The call this answers. v4 also repeats it in `source.callId`.
    pub tool_call_id: String,
    pub content: Vec<ContentBlock>,
    /// Absent means not an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh v4: `ToolMessageSource`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSource {
    pub kind: ToolKind,
    pub call_id: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Tokens counted for one model call. (dsh v4: `TokenUsage`.)
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Reads an optional field whose value may itself be `null`.
///
/// serde reads `null` as "absent" for an `Option`, and writing that back
/// would drop the field. Used with `default`, so a missing field is still
/// `None`, while a present `null` becomes `Some(Value::Null)`.
pub(crate) fn present<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}
