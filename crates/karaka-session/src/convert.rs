//! Between dsh v4's logged messages and Karaka's conversation types.
//!
//! Reading goes from a v4 message to what Karaka sends a model. Only text,
//! reasoning and tool calls survive it: Karaka's [`Block`] has nothing for
//! images, files or tool changes, so those stay in the log and are left out
//! of the history. That matches dsh's pi-ai adapter, which skips blocks it
//! can't send too (`userContent` and `foreignAssistant`).
//!
//! Writing goes from a finished model call or tool run to the v4 message the
//! loop logs. Each takes the message id from the caller, since this crate
//! has no id generator.
//!
//! # Reasoning signatures
//!
//! Karaka keeps a reasoning block's signature on [`Block::Reasoning`]'s
//! `replay`. v4 keeps it in the assistant message's
//! `source.replayState.blocks[i].thinkingSignature`, where `blocks` lines up
//! with `content` by index. dsh's pi-ai adapter (`replayedAssistant` in
//! `llm-pi-ai/src/replay.ts`) trusts that list only if it lines up exactly:
//! the same length as `content`, and each entry's `type` the same as its
//! block's. If not, it drops every signature in the message and sends the
//! blocks as plain history. Karaka reads it the same way.
//!
//! pi-ai also checks a `response` half of `replayState`, which names the
//! adapter, provider and model. Karaka doesn't write that half, since it
//! isn't pi-ai, so it doesn't require it. When the half is there, its
//! provider and model must match the message's source, as in pi-ai.

use karaka_model::{Block, Response, ToolCall, ToolResult, Usage};
use serde_json::{Value, json};

use crate::data::{AssistantMessageData, ToolResultData};
use crate::message::{
    AssistantMessage, AssistantRole, ContentBlock, Extra, ModelKind, ModelSource, PromptSource,
    ReasoningBlock, SystemMessage, SystemPromptSource, SystemRole, TokenUsage, ToolCallBlock,
    ToolKind, ToolResultMessage, ToolRole, ToolSource, UserMessage, UserRole, UserSource,
};

impl UserSource {
    /// dsh v4: `{ kind: "user" }`.
    pub fn user() -> Self {
        Self::User(PromptSource::default())
    }

    /// A prompt sent by the request `rpc_id`. (dsh v4: `{ kind: "user",
    /// rpcId }`.)
    pub fn prompt(rpc_id: impl Into<String>) -> Self {
        Self::User(PromptSource {
            rpc_id: Some(rpc_id.into()),
            ..PromptSource::default()
        })
    }

    /// The prompt's request id, if this is a person's prompt that has one.
    pub fn rpc_id(&self) -> Option<&str> {
        match self {
            Self::User(source) => source.rpc_id.as_deref(),
            _ => None,
        }
    }
}

impl UserMessage {
    /// A user message holding one piece of text.
    pub fn text(id: impl Into<String>, source: UserSource, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            role: UserRole::User,
            content: vec![ContentBlock::text(text)],
            source,
            extra: Extra::new(),
        }
    }

    /// The blocks a model reads: each text block, in order.
    pub fn blocks(&self) -> Vec<Block> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(Block::Text(text.text.clone())),
                _ => None,
            })
            .collect()
    }
}

impl SystemMessage {
    /// The rendered system prompt. Empty text records "no system prompt" as
    /// empty content, as v4's `createSystemMessage` does.
    pub fn new(id: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            id: id.into(),
            role: SystemRole::System,
            content: if text.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::text(text)]
            },
            source: SystemPromptSource::default(),
            extra: Extra::new(),
        }
    }
}

impl AssistantMessage {
    /// The v4 message for an answer the model at `provider` and `model`
    /// gave, with its reasoning signatures in `source.replayState`.
    pub fn from_model(
        id: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        content: &[Block],
    ) -> Self {
        let mut blocks = Vec::new();
        let mut replay = Vec::new();
        let mut signed = false;
        for block in content {
            match block {
                Block::Text(text) => {
                    blocks.push(ContentBlock::text(text.clone()));
                    replay.push(json!({ "type": "text" }));
                }
                Block::Reasoning {
                    text,
                    replay: signature,
                } => {
                    blocks.push(ContentBlock::Reasoning(ReasoningBlock {
                        text: text.clone(),
                        extra: Extra::new(),
                    }));
                    replay.push(match signature {
                        Some(signature) => {
                            signed = true;
                            json!({ "type": "reasoning", "thinkingSignature": signature })
                        }
                        None => json!({ "type": "reasoning" }),
                    });
                }
                Block::ToolCall(call) => {
                    blocks.push(ContentBlock::ToolCall(ToolCallBlock {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        extra: Extra::new(),
                    }));
                    replay.push(json!({ "type": "tool-call" }));
                }
                // A model never answers with a tool result; results live in
                // their own events. Skipping one here keeps `replay` lined up
                // with `blocks`, since neither gets an entry.
                Block::ToolResult(_) => {}
            }
        }
        Self {
            id: id.into(),
            role: AssistantRole::Assistant,
            content: blocks,
            source: ModelSource {
                kind: ModelKind::Model,
                provider: provider.into(),
                model: model.into(),
                // Only written when there is a signature to keep.
                replay_state: signed.then(|| json!({ "blocks": replay })),
                extra: Extra::new(),
            },
            extra: Extra::new(),
        }
    }

    /// The blocks a model reads, with reasoning signatures from
    /// `replayState` where it lines up. See the module docs for the rule.
    pub fn blocks(&self) -> Vec<Block> {
        let signatures = self.signatures();
        self.content
            .iter()
            .enumerate()
            .filter_map(|(index, block)| match block {
                ContentBlock::Text(text) => Some(Block::Text(text.text.clone())),
                ContentBlock::Reasoning(reasoning) => Some(Block::Reasoning {
                    text: reasoning.text.clone(),
                    replay: signatures
                        .as_ref()
                        .and_then(|signatures| signatures[index].clone()),
                }),
                ContentBlock::ToolCall(call) => Some(Block::ToolCall(ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })),
                ContentBlock::Other(_) => None,
            })
            .collect()
    }

    /// One entry per content block: the signature to send back with it.
    /// `None` when `replayState` is absent or doesn't line up, which drops
    /// every signature in the message.
    fn signatures(&self) -> Option<Vec<Option<String>>> {
        let state = self.source.replay_state.as_ref()?;
        if let Some(response) = state.get("response") {
            let names = |key| response.get(key).and_then(Value::as_str);
            if names("provider") != Some(&self.source.provider)
                || names("model") != Some(&self.source.model)
            {
                return None;
            }
        }
        let replay = state.get("blocks")?.as_array()?;
        if replay.len() != self.content.len() {
            return None;
        }
        self.content
            .iter()
            .zip(replay)
            .map(|(block, replay)| {
                let replay_type = replay.get("type").and_then(Value::as_str)?;
                let known = matches!(replay_type, "text" | "reasoning" | "tool-call");
                if !known || block.type_name() != Some(replay_type) {
                    return None;
                }
                let signature = replay.get("thinkingSignature");
                match signature {
                    None => Some(None),
                    Some(Value::String(signature)) => Some(Some(signature.clone())),
                    // pi-ai refuses a signature that isn't a string.
                    Some(_) => None,
                }
            })
            .collect()
    }
}

impl AssistantMessageData {
    /// The `assistant/message` data for a finished model call in step
    /// `step` of turn `turn`.
    ///
    /// The stream is written empty, since Karaka keeps no stream records,
    /// and v4 accepts that. The response's stop reason isn't written: v4
    /// keeps it only inside the stream and the replay state, so the loop
    /// acts on it live instead.
    pub fn from_response(
        id: impl Into<String>,
        turn: u64,
        step: u64,
        provider: impl Into<String>,
        model: impl Into<String>,
        response: &Response,
    ) -> Self {
        Self {
            turn,
            step,
            message: AssistantMessage::from_model(id, provider, model, &response.message.content),
            stream: Vec::new(),
            usage: Some(response.usage.into()),
            interrupted: false,
            extra: Extra::new(),
        }
    }
}

impl ToolResultMessage {
    /// The v4 message for what a tool returned. Its text is one text block.
    pub fn from_model(id: impl Into<String>, result: &ToolResult) -> Self {
        Self {
            id: id.into(),
            role: ToolRole::Tool,
            source: ToolSource {
                kind: ToolKind::Tool,
                call_id: result.call_id.clone(),
                extra: Extra::new(),
            },
            tool_call_id: result.call_id.clone(),
            content: vec![ContentBlock::text(result.content.clone())],
            is_error: Some(result.is_error),
            extra: Extra::new(),
        }
    }

    /// What the model reads: the text blocks joined, as pi-ai's
    /// `flattenText` joins them.
    pub fn to_model(&self) -> ToolResult {
        ToolResult {
            call_id: self.tool_call_id.clone(),
            content: joined_text(&self.content, ""),
            is_error: self.is_error.unwrap_or(false),
        }
    }
}

impl ToolResultData {
    /// The `tool/result` data for a tool run in step `step` of turn `turn`.
    pub fn new(id: impl Into<String>, turn: u64, step: u64, result: &ToolResult) -> Self {
        Self {
            turn,
            step,
            message: ToolResultMessage::from_model(id, result),
            error: None,
            meta: None,
            extra: Extra::new(),
        }
    }
}

/// Karaka's counts always include the cache fields, so they are always
/// written. A provider that reports no cache use writes 0.
impl From<Usage> for TokenUsage {
    fn from(usage: Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: None,
            cache_read_tokens: Some(usage.cache_read_tokens),
            cache_write_tokens: Some(usage.cache_write_tokens),
            reasoning_tokens: None,
            extra: Extra::new(),
        }
    }
}

/// The fields Karaka counts. `totalTokens` and `reasoningTokens` have no
/// place in [`Usage`].
impl From<&TokenUsage> for Usage {
    fn from(usage: &TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens.unwrap_or(0),
            cache_write_tokens: usage.cache_write_tokens.unwrap_or(0),
        }
    }
}

/// The text blocks of `content` joined by `separator`, other blocks left
/// out.
pub(crate) fn joined_text(content: &[ContentBlock], separator: &str) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(separator)
}
