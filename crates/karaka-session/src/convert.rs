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
//! `replay`, with the API, provider and model it came from. v4 keeps it in
//! the assistant message's `source.replayState`, which each of its adapters
//! writes in its own shape:
//!
//! ```json
//! // pi-ai (`llm-pi-ai/src/replay.ts:77-110`)
//! {"response": {"kind": "pi-ai", "version": 2, "api": "anthropic-messages",
//!               "provider": "anthropic", "model": "claude-opus-5-5",
//!               "stopReason": "toolUse"},
//!  "blocks": [{"type": "reasoning", "thinkingSignature": "..."},
//!             {"type": "tool-call"}]}
//!
//! // DeepSeek (`llm-deepseek/src/replay.ts:29-31`)
//! {"response": {"kind": "deepseek-messages", "version": 1,
//!               "model": "deepseek-flash"},
//!  "blocks": [{"type": "reasoning", "signature": "..."}, {"type": "text"}]}
//! ```
//!
//! `blocks` lines up with `content` by index. The `response` half says
//! where the signatures came from, so it gives each one its
//! [`Origin`]: pi-ai's names its API, provider and model, and DeepSeek's
//! is the `deepseek-messages` API, the message's provider and its model.
//!
//! Reading checks the state as strictly as v4 does
//! (`llm-pi-ai/src/replay.ts:117-149,187-194`, `llm-deepseek/src/replay.ts:48-64`).
//! A state that fails any check, such as a wrong kind or version, no
//! `response` half, a model or provider other than the message's source, or
//! blocks that don't line up, gives every signature in the message up. The
//! text, reasoning and tool calls still come back, as v4 keeps them
//! (`foreignAssistant`).
//!
//! Writing produces the state the adapter that matches the signatures'
//! origin would write, so v4 reads it as its own. It names that origin's
//! provider and model, and v4 compares those with the message's source: the
//! signatures survive only if the caller gave the same ones.

use karaka_model::{Block, Origin, Replay, Response, StopReason, ToolCall, ToolResult, Usage};
use serde_json::{Map, Value, json};

use crate::data::{AssistantMessageData, ToolResultData};
use crate::message::{
    AssistantMessage, AssistantRole, ContentBlock, Extra, ModelKind, ModelSource, PromptSource,
    ReasoningBlock, SystemMessage, SystemPromptSource, SystemRole, TokenUsage, ToolCallBlock,
    ToolKind, ToolResultMessage, ToolRole, ToolSource, UserMessage, UserRole, UserSource,
};

/// The kind of replay state dsh's pi-ai adapter writes, and its version.
const PI_AI: (&str, u64) = ("pi-ai", 2);

/// The kind of replay state dsh's DeepSeek adapter writes, and its version.
/// The kind is also the API in the origin of DeepSeek's signatures, as
/// `karaka-anthropic`'s DeepSeek profile names it.
const DEEPSEEK: (&str, u64) = ("deepseek-messages", 1);

/// The stop reasons pi-ai's state may name (`replay.ts:128`).
const PI_AI_STOP_REASONS: [&str; 5] = ["stop", "length", "toolUse", "error", "aborted"];

/// The block types a replay state may name (`replay.ts:139`).
const BLOCK_TYPES: [&str; 3] = ["text", "reasoning", "tool-call"];

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
    ///
    /// v4 trusts those signatures only when `provider` and `model` are the
    /// ones their origin names: the provider's own name, such as
    /// `anthropic`, and the model the request asked for.
    ///
    /// The state needs a stop reason. This guesses it from the content, as
    /// v4 does for a message with no state (`replay.ts:181`);
    /// [`AssistantMessageData::from_response`] uses the real one.
    pub fn from_model(
        id: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        content: &[Block],
    ) -> Self {
        let calls_tools = content.iter().any(|b| matches!(b, Block::ToolCall(_)));
        let stop = if calls_tools {
            StopReason::ToolCalls
        } else {
            StopReason::EndTurn
        };
        Self::answer(id, provider, model, content, stop)
    }

    fn answer(
        id: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        content: &[Block],
        stop: StopReason,
    ) -> Self {
        let mut blocks = Vec::new();
        // Each block's replay, lined up with `blocks`.
        let mut replays = Vec::new();
        for block in content {
            match block {
                Block::Text(text) => {
                    blocks.push(ContentBlock::text(text.clone()));
                    replays.push(None);
                }
                Block::Reasoning { text, replay } => {
                    blocks.push(ContentBlock::Reasoning(ReasoningBlock {
                        text: text.clone(),
                        extra: Extra::new(),
                    }));
                    replays.push(replay.as_ref());
                }
                Block::ToolCall(call) => {
                    blocks.push(ContentBlock::ToolCall(ToolCallBlock {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        extra: Extra::new(),
                    }));
                    replays.push(None);
                }
                // A model never answers with a tool result; results live in
                // their own events. Skipping one here keeps `replays` lined
                // up with `blocks`, since neither gets an entry.
                Block::ToolResult(_) => {}
            }
        }
        Self {
            id: id.into(),
            role: AssistantRole::Assistant,
            source: ModelSource {
                kind: ModelKind::Model,
                provider: provider.into(),
                model: model.into(),
                replay_state: replay_state(&blocks, &replays, stop),
                extra: Extra::new(),
            },
            content: blocks,
            extra: Extra::new(),
        }
    }

    /// The blocks a model reads, with reasoning signatures from
    /// `replayState` where v4 would trust it. See the module docs.
    pub fn blocks(&self) -> Vec<Block> {
        let mut replays = self.replays().ok();
        self.content
            .iter()
            .enumerate()
            .filter_map(|(index, block)| match block {
                ContentBlock::Text(text) => Some(Block::Text(text.text.clone())),
                ContentBlock::Reasoning(reasoning) => Some(Block::Reasoning {
                    text: reasoning.text.clone(),
                    replay: replays.as_mut().and_then(|replays| replays[index].take()),
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

    /// One entry per content block: the replay to send back with it. An
    /// error says why v4 wouldn't trust the state, in the words of its
    /// `invalid pi-ai replay state` and `DeepSeek Messages replay` errors.
    pub(crate) fn replays(&self) -> Result<Vec<Option<Replay>>, String> {
        let Some(state) = &self.source.replay_state else {
            return Err("no replay state".into());
        };
        let state = state.as_object().ok_or("expected a replay envelope")?;
        let response = state
            .get("response")
            .and_then(Value::as_object)
            .ok_or("expected a response object")?;
        match response.get("kind").and_then(Value::as_str) {
            Some(kind) if kind == PI_AI.0 => self.pi_ai_replays(response, state.get("blocks")),
            Some(kind) if kind == DEEPSEEK.0 => {
                self.deepseek_replays(response, state.get("blocks"))
            }
            _ => Err("unknown state kind".into()),
        }
    }

    /// A pi-ai state, read as `readReplayState` and `replayedAssistant`
    /// read it: first its shape, then whether it fits this message
    /// (`llm-pi-ai/src/replay.ts:117-149,187-194`).
    fn pi_ai_replays(
        &self,
        response: &Map<String, Value>,
        blocks: Option<&Value>,
    ) -> Result<Vec<Option<Replay>>, String> {
        if response.get("version").and_then(Value::as_u64) != Some(PI_AI.1) {
            let version = response
                .get("version")
                .map_or("undefined".into(), Value::to_string);
            return Err(format!("unsupported version {version}"));
        }
        let name = |key: &str| {
            response
                .get(key)
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| format!("{key} must be a non-empty string"))
        };
        let origin = Origin::new(name("api")?, name("provider")?, name("model")?);
        let stop = response.get("stopReason").and_then(Value::as_str);
        if !stop.is_some_and(|stop| PI_AI_STOP_REASONS.contains(&stop)) {
            return Err("unknown stopReason".into());
        }
        for key in ["responseModel", "responseId", "providerThinkingLevel"] {
            if response.get(key).is_some_and(|value| !value.is_string()) {
                return Err(format!("{key} must be a string"));
            }
        }
        let blocks = blocks
            .and_then(Value::as_array)
            .ok_or("blocks must be an array")?;
        let entries = blocks
            .iter()
            .enumerate()
            .map(|(index, block)| pi_ai_block(block, index))
            .collect::<Result<Vec<_>, _>>()?;

        if origin.provider != self.source.provider {
            return Err("provider does not match assistant source".into());
        }
        if origin.model != self.source.model {
            return Err("model does not match assistant source".into());
        }
        if entries.len() != self.content.len() {
            return Err("block count does not match assistant content".into());
        }
        self.content
            .iter()
            .zip(entries)
            .enumerate()
            .map(|(index, (block, (kind, signature)))| {
                if block.type_name() != Some(kind) {
                    return Err(format!("block {index} does not match assistant content"));
                }
                Ok(signature.map(|signature| Replay::new(origin.clone(), signature)))
            })
            .collect()
    }

    /// A DeepSeek state, read as `validateReplay` reads it
    /// (`llm-deepseek/src/replay.ts:48-64`). Its origin names the message's
    /// own provider: the state doesn't say one.
    fn deepseek_replays(
        &self,
        response: &Map<String, Value>,
        blocks: Option<&Value>,
    ) -> Result<Vec<Option<Replay>>, String> {
        if response.get("version").and_then(Value::as_u64) != Some(DEEPSEEK.1) {
            return Err("unsupported kind or version".into());
        }
        if response.get("model").and_then(Value::as_str) != Some(&self.source.model) {
            return Err("model does not match assistant source model".into());
        }
        let blocks = blocks
            .and_then(Value::as_array)
            .filter(|blocks| blocks.len() == self.content.len())
            .ok_or("block count mismatch")?;
        let origin = Origin::new(DEEPSEEK.0, &self.source.provider, &self.source.model);
        self.content
            .iter()
            .zip(blocks)
            .map(|(block, replay)| {
                let replay = replay.as_object().ok_or("expected a JSON object")?;
                let kind = replay.get("type").and_then(Value::as_str);
                if block.type_name() != kind || !kind.is_some_and(|k| BLOCK_TYPES.contains(&k)) {
                    return Err("block type mismatch".to_owned());
                }
                match replay.get("signature") {
                    None => Ok(None),
                    Some(Value::String(signature)) if kind == Some("reasoning") => {
                        Ok(Some(Replay::new(origin.clone(), signature)))
                    }
                    Some(_) => Err("invalid signature".to_owned()),
                }
            })
            .collect()
    }
}

/// One block of a pi-ai state: its type, and the reasoning signature to
/// send back, if any. Its fields are checked as `readReplayState` checks
/// them (`replay.ts:136-144`).
///
/// A block marked `redacted` holds encrypted thinking, which Anthropic takes
/// back only as a `redacted_thinking` block. Karaka can't send one, so its
/// signature is left out, and the block goes back as reasoning without it.
fn pi_ai_block(block: &Value, index: usize) -> Result<(&str, Option<String>), String> {
    let block = block
        .as_object()
        .ok_or_else(|| format!("block {index} must be an object"))?;
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| BLOCK_TYPES.contains(kind))
        .ok_or_else(|| format!("block {index} has an unknown type"))?;
    for key in [
        "textSignature",
        "thinkingSignature",
        "thoughtSignature",
        "namespace",
    ] {
        if block.get(key).is_some_and(|value| !value.is_string()) {
            return Err(format!("block {index} {key} must be a string"));
        }
    }
    let redacted = match block.get("redacted") {
        None => false,
        Some(Value::Bool(redacted)) => *redacted,
        Some(_) => return Err(format!("block {index} redacted must be boolean")),
    };
    // Only a reasoning block's signature is one Karaka sends back. The
    // others, `textSignature` and `thoughtSignature`, belong to OpenAI's
    // and Google's APIs, which Karaka doesn't speak.
    let signature = block
        .get("thinkingSignature")
        .and_then(Value::as_str)
        .filter(|_| kind == "reasoning" && !redacted)
        .map(str::to_owned);
    Ok((kind, signature))
}

/// The replay state for `blocks`, or `None` when no block has a replay.
///
/// Its origin is the first replay's. A reasoning block whose replay came
/// from somewhere else gets no signature in it: one state names one origin.
fn replay_state(
    blocks: &[ContentBlock],
    replays: &[Option<&Replay>],
    stop: StopReason,
) -> Option<Value> {
    let origin = &replays.iter().flatten().next()?.origin;
    let deepseek = origin.api == DEEPSEEK.0;
    let entries: Vec<Value> = blocks
        .iter()
        .zip(replays)
        .map(|(block, replay)| {
            let mut entry = Map::new();
            entry.insert("type".into(), json!(block.type_name()));
            if let Some(replay) = replay.filter(|replay| &replay.origin == origin) {
                let key = if deepseek {
                    "signature"
                } else {
                    "thinkingSignature"
                };
                entry.insert(key.into(), json!(replay.signature));
            }
            Value::Object(entry)
        })
        .collect();
    let response = if deepseek {
        // `llm-deepseek/src/replay.ts:30`.
        json!({ "kind": DEEPSEEK.0, "version": DEEPSEEK.1, "model": origin.model })
    } else {
        // `llm-pi-ai/src/replay.ts:78-88`, without the optional fields
        // Karaka doesn't know: `responseModel`, `responseId` and
        // `providerThinkingLevel`.
        json!({
            "kind": PI_AI.0,
            "version": PI_AI.1,
            "api": origin.api,
            "provider": origin.provider,
            "model": origin.model,
            "stopReason": pi_ai_stop_reason(stop),
        })
    };
    Some(json!({ "response": response, "blocks": entries }))
}

/// pi-ai's name for a stop reason. It calls a paused turn a stop
/// (`anthropic-messages.js:1298-1299`).
fn pi_ai_stop_reason(stop: StopReason) -> &'static str {
    match stop {
        StopReason::EndTurn | StopReason::Paused => "stop",
        StopReason::ToolCalls => "toolUse",
        StopReason::MaxTokens => "length",
    }
}

impl AssistantMessageData {
    /// The `assistant/message` data for a finished model call in step
    /// `step` of turn `turn`.
    ///
    /// The stream is written empty, since Karaka keeps no stream records,
    /// and v4 accepts that. The response's stop reason goes only into the
    /// replay state, the one place outside the stream where v4 keeps it.
    /// See [`AssistantMessage::from_model`] for `provider` and `model`.
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
            message: AssistantMessage::answer(
                id,
                provider,
                model,
                &response.message.content,
                response.stop,
            ),
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

#[cfg(test)]
mod tests {
    use karaka_model::{
        Block, Message, Origin, Replay, Response, Role, StopReason, ToolCall, Usage,
    };
    use serde_json::{Value, json};

    use super::{AssistantMessage, AssistantMessageData};

    fn anthropic(model: &str, signature: &str) -> Option<Replay> {
        Some(Replay::new(
            Origin::new("anthropic-messages", "anthropic", model),
            signature,
        ))
    }

    fn deepseek(model: &str, signature: &str) -> Option<Replay> {
        Some(Replay::new(
            Origin::new("deepseek-messages", "deepseek", model),
            signature,
        ))
    }

    fn reasoning(text: &str, replay: Option<Replay>) -> Block {
        Block::Reasoning {
            text: text.into(),
            replay,
        }
    }

    fn call() -> Block {
        Block::ToolCall(ToolCall {
            id: "c1".into(),
            name: "clock".into(),
            arguments: "{}".into(),
        })
    }

    /// `content` as a message from `provider` and `model` whose replay
    /// state is `state`, read back.
    fn with_state(provider: &str, model: &str, content: &[Block], state: Value) -> Vec<Block> {
        let mut message = AssistantMessage::from_model("m", provider, model, content);
        message.source.replay_state = Some(state);
        message.blocks()
    }

    /// The reason `state` fails, for a message holding one text block, as
    /// dsh's pi-ai test `expectDegraded` builds it (`convert.spec.ts:618-640`).
    fn pi_ai_problem(state: Value) -> String {
        let done = [Block::Text("done".into())];
        let mut message = AssistantMessage::from_model("m", "deepseek", "deepseek-v4-flash", &done);
        message.source.replay_state = Some(state);
        // The text survives whatever the state holds.
        assert_eq!(message.blocks(), [Block::Text("done".into())]);
        message.replays().expect_err("the state is refused")
    }

    // Writing.

    /// What Karaka writes for an Anthropic answer: pi-ai's v2 state, with the
    /// `response` half v4 needs, in the order pi-ai writes its fields
    /// (`replay.ts:78-88`).
    #[test]
    fn an_anthropic_answer_gets_the_replay_state_pi_ai_writes() {
        let content = [
            reasoning("thinking", anthropic("claude-opus-5-5", "sig-1")),
            Block::Text("answer".into()),
            call(),
        ];
        let written = AssistantMessage::from_model("m", "anthropic", "claude-opus-5-5", &content);
        assert_eq!(
            serde_json::to_string(&written.source.replay_state).unwrap(),
            json!({
                "response": {
                    "kind": "pi-ai",
                    "version": 2,
                    "api": "anthropic-messages",
                    "provider": "anthropic",
                    "model": "claude-opus-5-5",
                    "stopReason": "toolUse"
                },
                "blocks": [
                    {"type": "reasoning", "thinkingSignature": "sig-1"},
                    {"type": "text"},
                    {"type": "tool-call"}
                ]
            })
            .to_string()
        );
        // It passes every check v4 makes, and reads back as it was written.
        assert!(written.replays().is_ok());
        assert_eq!(written.blocks(), content);
    }

    /// What Karaka writes for a DeepSeek answer: dsh's DeepSeek state
    /// (`llm-deepseek/src/replay.ts:29-31`).
    #[test]
    fn a_deepseek_answer_gets_the_replay_state_dshs_deepseek_adapter_writes() {
        let content = [
            reasoning("", deepseek("deepseek-flash", "opaque")),
            Block::Text("answer".into()),
        ];
        let written = AssistantMessage::from_model("m", "deepseek", "deepseek-flash", &content);
        assert_eq!(
            serde_json::to_string(&written.source.replay_state).unwrap(),
            json!({
                "response": {"kind": "deepseek-messages", "version": 1, "model": "deepseek-flash"},
                "blocks": [{"type": "reasoning", "signature": "opaque"}, {"type": "text"}]
            })
            .to_string()
        );
        assert!(written.replays().is_ok());
        assert_eq!(written.blocks(), content);
    }

    /// A finished call's own stop reason goes in the state; `from_model`
    /// can only guess one.
    #[test]
    fn the_state_names_the_calls_stop_reason() {
        let response = Response {
            message: Message {
                role: Role::Assistant,
                content: vec![reasoning("cut off", anthropic("claude-opus-5-5", "sig"))],
            },
            stop: StopReason::MaxTokens,
            usage: Usage::default(),
        };
        let data = AssistantMessageData::from_response(
            "m",
            1,
            1,
            "anthropic",
            "claude-opus-5-5",
            &response,
        );
        let state = data.message.source.replay_state.expect("a state");
        assert_eq!(state["response"]["stopReason"], "length");
    }

    #[test]
    fn an_answer_with_no_replay_gets_no_state() {
        let written = AssistantMessage::from_model(
            "m",
            "openai",
            "gpt-test",
            &[reasoning("unsigned", None), Block::Text("answer".into())],
        );
        assert_eq!(written.source.replay_state, None);
    }

    /// One state names one origin, so a signature from another one is left
    /// out of it, and its reasoning reads back unsigned.
    #[test]
    fn a_signature_from_another_origin_is_left_out_of_the_state() {
        let content = [
            reasoning("mine", anthropic("claude-opus-5-5", "sig")),
            reasoning("theirs", deepseek("deepseek-flash", "other")),
        ];
        let written = AssistantMessage::from_model("m", "anthropic", "claude-opus-5-5", &content);
        assert_eq!(
            written.source.replay_state.as_ref().unwrap()["blocks"],
            json!([
                {"type": "reasoning", "thinkingSignature": "sig"},
                {"type": "reasoning"}
            ])
        );
        assert_eq!(written.blocks()[1], reasoning("theirs", None));
    }

    /// v4 compares the state's provider and model with the message's
    /// source, so a caller that names another provider loses the
    /// signatures, there and here.
    #[test]
    fn a_source_other_than_the_signatures_origin_loses_them() {
        let content = [reasoning("thinking", anthropic("claude-opus-5-5", "sig"))];
        let written =
            AssistantMessage::from_model("m", "anthropic-work", "claude-opus-5-5", &content);
        assert_eq!(
            written.replays(),
            Err("provider does not match assistant source".into())
        );
        assert_eq!(written.blocks(), [reasoning("thinking", None)]);
    }

    // Reading.

    /// Play This Life's logs hold pi-ai states of this shape, with the
    /// optional fields pi-ai writes for Anthropic. The values here are
    /// made up.
    #[test]
    fn a_pi_ai_state_gives_each_signature_its_origin() {
        let content = [reasoning("thinking", None), call()];
        let state = json!({
            "response": {
                "kind": "pi-ai", "version": 2, "api": "anthropic-messages",
                "provider": "anthropic", "model": "claude-haiku-5-5",
                "responseId": "msg_made_up", "providerThinkingLevel": "medium",
                "stopReason": "toolUse"
            },
            "blocks": [{"type": "reasoning", "thinkingSignature": "sig"}, {"type": "tool-call"}]
        });
        assert_eq!(
            with_state("anthropic", "claude-haiku-5-5", &content, state),
            [
                reasoning("thinking", anthropic("claude-haiku-5-5", "sig")),
                call()
            ]
        );
    }

    /// dsh's DeepSeek state names no provider, so the origin takes the
    /// message's own.
    #[test]
    fn a_deepseek_state_gives_each_signature_its_origin() {
        let content = [reasoning("", None), Block::Text("answer".into())];
        let state = json!({
            "response": {"kind": "deepseek-messages", "version": 1, "model": "deepseek-v4-flash"},
            "blocks": [{"type": "reasoning", "signature": "signed"}, {"type": "text"}]
        });
        assert_eq!(
            with_state("deepseek-official", "deepseek-v4-flash", &content, state)[0],
            reasoning(
                "",
                Some(Replay::new(
                    Origin::new(
                        "deepseek-messages",
                        "deepseek-official",
                        "deepseek-v4-flash"
                    ),
                    "signed"
                ))
            )
        );
    }

    /// The state Karaka wrote before it kept origins had no `response`
    /// half. v4 refuses that, and so does Karaka now.
    #[test]
    fn a_state_with_no_response_half_loses_its_signatures() {
        let content = [reasoning("thinking", None), Block::Text("answer".into())];
        let state = json!({ "blocks": [
            { "type": "reasoning", "thinkingSignature": "sig-1" },
            { "type": "text" },
        ] });
        assert_eq!(with_state("anthropic", "claude", &content, state), content);
    }

    /// A pi-ai state with a signature, changed one way at a time: each
    /// change loses the signature, and only the signature.
    #[test]
    fn a_pi_ai_state_of_another_kind_or_version_loses_its_signatures() {
        let content = [reasoning("thinking", None), Block::Text("answer".into())];
        let state = |kind: &str, version: u64| {
            json!({
                "response": {"kind": kind, "version": version, "api": "anthropic-messages",
                             "provider": "anthropic", "model": "claude-opus-5-5",
                             "stopReason": "stop"},
                "blocks": [{"type": "reasoning", "thinkingSignature": "sig"}, {"type": "text"}]
            })
        };
        let read = |state| with_state("anthropic", "claude-opus-5-5", &content, state);
        assert_eq!(
            read(state("pi-ai", 2))[0],
            reasoning("thinking", anthropic("claude-opus-5-5", "sig"))
        );
        for (kind, version) in [
            ("pi-ai", 1),
            ("pi-ai", 3),
            ("other", 2),
            ("deepseek-messages", 2),
        ] {
            assert_eq!(read(state(kind, version)), content, "{kind} {version}");
        }
    }

    /// dsh's pi-ai "degrades replay metadata whose %s differs from assistant
    /// source", "degrades unsupported replay-state versions", "degrades the
    /// flat pre-envelope replay state", and "degrades malformed replay
    /// state" (`convert.spec.ts:511-685`), with v4's own reasons.
    #[test]
    fn a_pi_ai_state_v4_would_refuse_is_refused_for_the_same_reason() {
        let response = json!({
            "kind": "pi-ai", "version": 2, "api": "openai-completions",
            "provider": "deepseek", "model": "deepseek-v4-flash", "stopReason": "stop"
        });
        let valid = json!({ "response": response, "blocks": [{ "type": "text" }] });
        let changed = |key: &str, value: Value| {
            let mut state = valid.clone();
            if value.is_null() && key == "model" {
                state["response"].as_object_mut().unwrap().remove(key);
            } else {
                state["response"][key] = value;
            }
            state
        };
        let blocks = |blocks: Value| json!({ "response": response, "blocks": blocks });
        let cases = [
            (
                changed("provider", json!("openai")),
                "provider does not match assistant source",
            ),
            (
                changed("model", json!("deepseek-v4-pro")),
                "model does not match assistant source",
            ),
            (changed("version", json!(3)), "unsupported version 3"),
            (
                json!({ "kind": "pi-ai", "version": 1, "api": "openai-completions",
                        "provider": "deepseek", "model": "deepseek-v4-flash",
                        "stopReason": "stop", "blocks": [{ "type": "text" }] }),
                "expected a response object",
            ),
            (json!(1), "expected a replay envelope"),
            (Value::Null, "expected a replay envelope"),
            (json!([]), "expected a replay envelope"),
            (json!({ "blocks": [] }), "expected a response object"),
            (
                json!({ "response": [], "blocks": [] }),
                "expected a response object",
            ),
            (changed("kind", json!("other")), "unknown state kind"),
            (changed("api", json!(1)), "api must be a non-empty string"),
            (
                changed("provider", json!("")),
                "provider must be a non-empty string",
            ),
            (
                changed("model", Value::Null),
                "model must be a non-empty string",
            ),
            (changed("stopReason", json!("pause")), "unknown stopReason"),
            (
                changed("responseModel", json!(1)),
                "responseModel must be a string",
            ),
            (
                changed("responseId", json!(1)),
                "responseId must be a string",
            ),
            (
                changed("providerThinkingLevel", json!(1)),
                "providerThinkingLevel must be a string",
            ),
            (
                changed("providerThinkingLevel", Value::Null),
                "providerThinkingLevel must be a string",
            ),
            (json!({ "response": response }), "blocks must be an array"),
            (blocks(json!("text")), "blocks must be an array"),
            (blocks(json!([1])), "block 0 must be an object"),
            (blocks(json!([null])), "block 0 must be an object"),
            (blocks(json!([[]])), "block 0 must be an object"),
            (
                blocks(json!([{ "type": "audio" }])),
                "block 0 has an unknown type",
            ),
            (
                blocks(json!([{ "type": "tool-call", "namespace": 1 }])),
                "block 0 namespace must be a string",
            ),
            (
                blocks(json!([{ "type": "text", "textSignature": 1 }])),
                "block 0 textSignature must be a string",
            ),
            (
                blocks(json!([{ "type": "reasoning", "redacted": "yes" }])),
                "block 0 redacted must be boolean",
            ),
            (
                blocks(json!([{ "type": "reasoning" }])),
                "block 0 does not match assistant content",
            ),
            (
                blocks(json!([{ "type": "text" }, { "type": "text" }])),
                "block count does not match assistant content",
            ),
        ];
        for (state, reason) in cases {
            assert_eq!(pi_ai_problem(state.clone()), reason, "{state}");
        }
        // And the state they all start from passes.
        let mut message = AssistantMessage::from_model("m", "deepseek", "deepseek-v4-flash", &[]);
        message.content = vec![crate::message::ContentBlock::text("done")];
        message.source.replay_state = Some(valid);
        assert!(message.replays().is_ok());
    }

    /// dsh's DeepSeek "degrades unusable replay state with a diagnostic"
    /// (`llm-deepseek/tests/serialize.spec.ts:367-386`): each of these
    /// loses the signature, and the reasoning comes back unsigned.
    #[test]
    fn a_deepseek_state_dsh_would_refuse_loses_its_signatures() {
        const MODEL: &str = "deepseek-v4-flash";
        let content = [reasoning("think", None)];
        let response = json!({ "kind": "deepseek-messages", "version": 1, "model": MODEL });
        let states = [
            Value::Null,
            json!([]),
            json!({ "response": null, "blocks": [] }),
            json!({ "response": { "kind": "other", "version": 1 }, "blocks": [] }),
            json!({ "response": { "kind": "deepseek-messages", "version": 2 }, "blocks": [] }),
            json!({ "response": { "kind": "deepseek-messages", "version": 1, "model": "wrong" },
                    "blocks": [{ "type": "reasoning", "signature": "s" }] }),
            json!({ "response": response, "blocks": [] }),
            json!({ "response": response, "blocks": null }),
            json!({ "response": response, "blocks": [null] }),
            json!({ "response": response, "blocks": [{ "type": "tool-call" }] }),
            json!({ "response": response, "blocks": [{ "type": "reasoning", "signature": 3 }] }),
        ];
        for state in states {
            assert_eq!(
                with_state("deepseek-official", MODEL, &content, state.clone()),
                content,
                "{state}"
            );
        }
    }

    /// dsh's DeepSeek "keeps durable content when replay degrades": a
    /// signature on a text block spoils the whole state.
    #[test]
    fn a_deepseek_signature_on_a_text_block_spoils_the_state() {
        let content = [
            reasoning("Read the file.", None),
            Block::Text("Checking a.".into()),
            call(),
        ];
        let state = json!({
            "response": {"kind": "deepseek-messages", "version": 1, "model": "deepseek-v4-flash"},
            "blocks": [
                {"type": "reasoning", "signature": "do-not-send"},
                {"type": "text", "signature": "invalid-for-text"},
                {"type": "tool-call"}
            ]
        });
        let mut message =
            AssistantMessage::from_model("m", "deepseek", "deepseek-v4-flash", &content);
        message.source.replay_state = Some(state);
        assert_eq!(message.replays(), Err("invalid signature".into()));
        assert_eq!(message.blocks(), content);
    }

    /// pi-ai sends redacted thinking back as `redacted_thinking`, which
    /// Karaka can't, so that block's signature is dropped and the rest kept.
    #[test]
    fn redacted_thinking_loses_only_its_own_signature() {
        let content = [reasoning("", None), reasoning("visible", None)];
        let state = json!({
            "response": {"kind": "pi-ai", "version": 2, "api": "anthropic-messages",
                         "provider": "anthropic", "model": "claude-opus-5-5", "stopReason": "stop"},
            "blocks": [
                {"type": "reasoning", "thinkingSignature": "encrypted", "redacted": true},
                {"type": "reasoning", "thinkingSignature": "sig"}
            ]
        });
        assert_eq!(
            with_state("anthropic", "claude-opus-5-5", &content, state),
            [
                reasoning("", None),
                reasoning("visible", anthropic("claude-opus-5-5", "sig"))
            ]
        );
    }
}
