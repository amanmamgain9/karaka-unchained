use std::fmt;

use karaka_model::{Block, Response, StopReason, ToolResult, Usage};

/// One thing that happened in a session, as the log records it.
/// (dsh: `SessionEventMap` in `types.ts`.)
///
/// There are only the three events a conversation with tool calls needs, and
/// all three produce messages for the model. They are dsh's three "surface"
/// events, the only ones dsh's `deriveMessages` reads.
///
/// # Left out on purpose
///
/// dsh logs much more. None of it is needed to build the model's history, so
/// none of it is here yet:
///
/// - `turn/start`, `turn/end`, `step/start`, `step/end`: dsh uses them to
///   check the log, to fork only at a closed turn, and to scope repair to the
///   open turn. Derivation never reads them. Repair here scopes a tool call
///   by the assistant message that made it instead (see `repair.rs`).
/// - `assistant/chunk`: raw streamed tokens, for token-exact replay.
/// - `tool/call`: dsh logs a call when the tool starts. Repair uses it to
///   tell a call that never started from one whose outcome is unknown.
///   Without it, every interrupted call here is treated as outcome unknown.
/// - `request/header`, `request/context`: the system prompt, tools and
///   route, so each request can be rebuilt from the log.
/// - `session/end-seed`: marks where a resumed or forked log's seed ends.
/// - every plugin event, such as compaction, approvals and todos.
/// - on the events kept: the assistant message's `interrupted` marker, the
///   tool result's `error` and `meta`, and the envelope's `time`,
///   `ignorable`, `surfaceOp` and `sourceEventSeqs`.
///
/// # Adding events later
///
/// dsh plugins add events by declaration merging: each plugin adds keys to
/// one shared TypeScript interface. Rust has no such thing, since an enum is
/// closed once it's written. So new core events, such as turn boundaries,
/// will be new variants here. `#[non_exhaustive]` makes other crates match
/// with a `_` arm, so adding one doesn't break them. Plugin events will
/// later get one variant that holds a kind name and opaque serialized data.
/// The log stores it and derivation skips it, the way dsh skips events that
/// aren't on the surface. That variant waits for serde, since its data is
/// serialized.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum SessionEvent {
    /// Something said in the user role: a person's prompt, or context the
    /// harness adds. (dsh: `user/message`.)
    ///
    /// Tool results don't go here: each one is its own
    /// [`SessionEvent::ToolResult`], so derivation can group them and repair
    /// can see which calls were answered.
    UserMessage { content: Vec<Block> },
    /// One model call's answer, with why it stopped and what it cost.
    /// (dsh: `assistant/message`.)
    ///
    /// The usage lives on this event, as in dsh, so a model's output and its
    /// cost are recorded together and there is no separate usage event.
    AssistantMessage {
        content: Vec<Block>,
        stop: StopReason,
        usage: Usage,
    },
    /// What one tool call returned. (dsh: `tool/result`.)
    ToolResult(ToolResult),
}

impl SessionEvent {
    /// A user message holding one piece of text.
    pub fn user(text: impl Into<String>) -> Self {
        Self::UserMessage {
            content: vec![Block::Text(text.into())],
        }
    }
}

/// A finished model call becomes the assistant event that records it.
///
/// The event keeps the content but not the role, which is always assistant.
/// That way an assistant event can't hold a message in the wrong role.
impl From<Response> for SessionEvent {
    fn from(response: Response) -> Self {
        Self::AssistantMessage {
            content: response.message.content,
            stop: response.stop,
            usage: response.usage,
        }
    }
}

/// An event's position in its session's log.
///
/// The first event is 0 and each append adds 1, with no gaps, so an event's
/// seq is also its index in the log. dsh starts at 0 too, and its persistence
/// backends rely on that: a backend checks that a batch's first seq is the
/// next one it expects. (dsh: `seq = log.length`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(pub u64);

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One event in the log, with its position.
///
/// The seq is stored even though it equals the index, because that's what a
/// backend will write: each stored event says where it belongs.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub seq: Seq,
    pub event: SessionEvent,
}
