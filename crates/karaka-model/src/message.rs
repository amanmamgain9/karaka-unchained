/// Who said a message.
///
/// There is no system role: the system prompt is part of the request, not of
/// the conversation, because providers such as Anthropic take it separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
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
        /// Opaque data the provider attached to this reasoning, sent back to
        /// the same provider unchanged. Anthropic uses it to check that the
        /// reasoning wasn't edited before it accepts it again.
        replay: Option<String>,
    },
    /// The model asking for a tool to run. Only in assistant messages.
    ToolCall(ToolCall),
    /// What a tool returned. Only in user messages.
    ToolResult(ToolResult),
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
