/// Who said a message.
///
/// The system prompt a conversation starts with isn't a message: it is
/// [`Request::system`](crate::Request::system), because providers such as
/// Anthropic take it separately. Instructions that arrive later keep their
/// place in the history instead, as dsh v4 keeps them
/// (`llm-pi-ai/src/context.ts:148-158,228-231`). Each provider sends them in
/// place where its model accepts that, and otherwise folds them into the
/// system prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    /// A system prompt that took effect partway through the conversation.
    /// It replaces the one before it, so the newest one is the prompt in
    /// effect. Only text blocks.
    System,
    /// The harness telling the model what changed in its session, such as
    /// its tools. dsh calls these developer messages. Only text blocks.
    ///
    /// This isn't OpenAI's `developer` role, which is its new name for
    /// `system`. Most providers send both roles the same way. dsh's DeepSeek
    /// adapter is the exception: it can fold a system prompt into the request,
    /// but always sends developer messages in place
    /// (`llm-deepseek/src/serialize.ts:91-116`).
    Developer,
}

/// One turn of a conversation: a role and the blocks it said, in order.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

impl Message {
    /// A user message holding one piece of text.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![Block::Text(text.into())],
        }
    }

    /// A system prompt that takes effect at this point in the conversation.
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: vec![Block::Text(text.into())],
        }
    }

    /// The message's visible text, with its text blocks joined in order.
    /// Reasoning and tool blocks are left out.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                Block::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// One piece of a message.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    /// Text the end user sees.
    Text(String),
    /// The model thinking before it answers.
    Reasoning {
        text: String,
        /// Opaque data the provider attached to this reasoning, and where it
        /// came from. `None` when the provider attached nothing.
        replay: Option<Replay>,
    },
    /// The model asking for a tool to run. Only in assistant messages.
    ToolCall(ToolCall),
    /// What a tool returned. Only in user messages.
    ToolResult(ToolResult),
}

/// Opaque data a provider attached to a reasoning block, such as Anthropic's
/// signature, kept with where it came from.
///
/// A provider sends the data back only to the API, provider and model that
/// wrote it, as pi-ai does (`api/transform-messages.js:66-91`). A signature
/// is bound to its model: Anthropic checks it before it accepts the
/// reasoning again, and refuses one it didn't issue. Reasoning whose replay
/// doesn't match goes back without it, in whatever form the provider takes
/// unsigned reasoning, so what the model worked out isn't lost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replay {
    pub origin: Origin,
    /// Sent back unchanged. It may be empty: Anthropic can send an empty
    /// signature, and DeepSeek may sign with one.
    pub signature: String,
}

impl Replay {
    pub fn new(origin: Origin, signature: impl Into<String>) -> Self {
        Self {
            origin,
            signature: signature.into(),
        }
    }
}

/// Which API, provider and model wrote a piece of replay data. pi-ai compares
/// these three before it replays a signature
/// (`api/transform-messages.js:68-70`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    /// The wire format, in pi-ai's names, such as `anthropic-messages`. dsh's
    /// DeepSeek adapter has a name of its own, `deepseek-messages`
    /// (`llm-deepseek/src/replay.ts:30`).
    pub api: String,
    /// Who served the call, such as `anthropic`.
    pub provider: String,
    /// The model the request asked for. A provider may report a different
    /// one, such as a dated snapshot, but the request's is what the next
    /// request names, so that is what gets compared (`llm-pi-ai/src/replay.ts:27-30`).
    pub model: String,
}

impl Origin {
    pub fn new(
        api: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            api: api.into(),
            provider: provider.into(),
            model: model.into(),
        }
    }
}

/// A request from the model to run one tool.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    /// Chosen by the provider; the matching [`ToolResult`] repeats it.
    pub id: String,
    pub name: String,
    /// The arguments as the model wrote them: JSON, but not checked. Models
    /// sometimes write broken JSON, and the tool reports that, not the model.
    pub arguments: String,
}

/// The outcome of one [`ToolCall`], sent back to the model.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    /// The [`ToolCall::id`] this answers.
    pub call_id: String,
    pub content: String,
    /// Whether the tool failed. The model still reads `content`, so it can
    /// see what went wrong and try again.
    pub is_error: bool,
}

#[cfg(test)]
mod tests {
    use super::{Block, Message, Role, ToolCall};

    #[test]
    fn a_user_message_holds_its_text() {
        let message = Message::user("hello");
        assert_eq!(message.role, Role::User);
        assert_eq!(message.content, [Block::Text("hello".into())]);
    }

    #[test]
    fn a_system_message_holds_its_text() {
        let message = Message::system("be brief");
        assert_eq!(message.role, Role::System);
        assert_eq!(message.text(), "be brief");
    }

    #[test]
    fn text_skips_reasoning_and_tool_calls() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                Block::Reasoning {
                    text: "they want the time".into(),
                    replay: None,
                },
                Block::Text("Let me check. ".into()),
                Block::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "clock".into(),
                    arguments: "{}".into(),
                }),
                Block::Text("One moment.".into()),
            ],
        };
        assert_eq!(message.text(), "Let me check. One moment.");
    }
}
