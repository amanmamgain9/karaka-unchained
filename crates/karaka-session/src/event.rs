use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::data::{
    AssistantMessageData, DeveloperMessageData, Step, SystemMessageData, ToolCallData,
    ToolResultData, TurnEnd, TurnEndReason, TurnStart,
};
use crate::message::{Extra, SystemMessage, UserMessage};
use crate::request::{
    AssistantAttemptData, RequestContext, RequestHeader, RequestHeaderData, RequestHeaderReason,
};

/// One thing that happened in a session, as the log records it.
/// (dsh v4: `SessionEventMap` in `types.ts`.)
///
/// Each variant names the v4 event type it is. The JSON form is v4's
/// envelope exactly; see `json.rs`.
///
/// # Three groups
///
/// - **Surface events**, the five that produce messages for the model:
///   system, developer, user, assistant and tool result. Each says how it
///   joins the model-visible surface (see [`SurfaceIntent`]).
/// - **Lifecycle events**, which Karaka's loop writes and reads: turn and
///   step boundaries, `tool/call`, and the three it logs around each model
///   call, `request/header`, `request/context` and `assistant/attempt`.
///   They never change the history the model sees.
/// - **[`Passthrough`]**, for every other type: approvals, questions,
///   compaction, plugin events, and whatever comes next. Karaka keeps the type and payload untouched, so a
///   v4 log keeps every event, and every event keeps its seq.
///
/// dsh plugins add event types by merging keys into one TypeScript
/// interface. A Rust enum is closed, so new types Karaka needs become new
/// variants, and the rest stay passthrough. `#[non_exhaustive]` makes other
/// crates match with a `_` arm, so a new variant doesn't break them.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum SessionEvent {
    /// The rendered system prompt. The first one is surface node 0.
    /// (dsh v4: `system/message`.)
    SystemMessage {
        data: SystemMessageData,
        surface: SurfaceIntent,
    },
    /// A change to the session, such as tools added or removed.
    /// (dsh v4: `developer/message`.)
    DeveloperMessage {
        data: DeveloperMessageData,
        surface: SurfaceIntent,
    },
    /// A person's prompt, or context the harness injects.
    /// (dsh v4: `user/message`.)
    ///
    /// Unlike the other surface events, v4 logs the message itself as the
    /// data, with no turn or step.
    UserMessage {
        message: UserMessage,
        surface: SurfaceIntent,
    },
    /// One model call's answer, with its usage. (dsh v4: `assistant/message`.)
    ///
    /// Only an operation, not a full [`SurfaceIntent`]: v4 forbids an
    /// assistant message from citing source events, because it embeds the
    /// stream it came from instead.
    AssistantMessage {
        data: AssistantMessageData,
        op: SurfaceOp,
    },
    /// What one tool call returned. (dsh v4: `tool/result`.)
    ToolResult {
        data: ToolResultData,
        surface: SurfaceIntent,
    },
    /// dsh v4: `turn/start`.
    TurnStart(TurnStart),
    /// dsh v4: `turn/end`.
    TurnEnd(TurnEnd),
    /// dsh v4: `step/start`.
    StepStart(Step),
    /// dsh v4: `step/end`.
    StepEnd(Step),
    /// A tool started running. (dsh v4: `tool/call`.)
    ToolCall(ToolCallData),
    /// The settings and tools of the next request. (dsh v4: `request/header`.)
    RequestHeader(RequestHeaderData),
    /// The route of the next request, when it changed.
    /// (dsh v4: `request/context`.)
    RequestContext(RequestContext),
    /// A model call that left no message. (dsh v4: `assistant/attempt`.)
    AssistantAttempt(AssistantAttemptData),
    /// Any other event type, kept as it was.
    Passthrough(Passthrough),
}

impl SessionEvent {
    /// The v4 event type, such as `"turn/start"`.
    pub fn event_type(&self) -> &str {
        match self {
            Self::SystemMessage { .. } => "system/message",
            Self::DeveloperMessage { .. } => "developer/message",
            Self::UserMessage { .. } => "user/message",
            Self::AssistantMessage { .. } => "assistant/message",
            Self::ToolResult { .. } => "tool/result",
            Self::TurnStart(_) => "turn/start",
            Self::TurnEnd(_) => "turn/end",
            Self::StepStart(_) => "step/start",
            Self::StepEnd(_) => "step/end",
            Self::ToolCall(_) => "tool/call",
            Self::RequestHeader(_) => "request/header",
            Self::RequestContext(_) => "request/context",
            Self::AssistantAttempt(_) => "assistant/attempt",
            Self::Passthrough(passthrough) => &passthrough.event_type,
        }
    }

    /// A user message, added to the end of the surface.
    pub fn user(message: UserMessage) -> Self {
        Self::UserMessage {
            message,
            surface: SurfaceIntent::append(),
        }
    }

    /// A system prompt, added to the end of the surface.
    pub fn system(turn: u64, step: u64, message: SystemMessage) -> Self {
        Self::SystemMessage {
            data: SystemMessageData {
                turn,
                step,
                message,
                extra: Extra::new(),
            },
            surface: SurfaceIntent::append(),
        }
    }

    /// An assistant message, added to the end of the surface.
    pub fn assistant(data: AssistantMessageData) -> Self {
        Self::AssistantMessage {
            data,
            op: SurfaceOp::Append,
        }
    }

    /// A tool result, added to the end of the surface. It cites the
    /// `tool/call` event at `call_seq`, as v4's loop does.
    pub fn tool_result(data: ToolResultData, call_seq: Seq) -> Self {
        Self::ToolResult {
            data,
            surface: SurfaceIntent {
                op: SurfaceOp::Append,
                sources: vec![call_seq],
            },
        }
    }

    pub fn turn_start(turn: u64) -> Self {
        Self::TurnStart(TurnStart {
            turn,
            extra: Extra::new(),
        })
    }

    pub fn turn_end(turn: u64, reason: TurnEndReason) -> Self {
        Self::TurnEnd(TurnEnd {
            turn,
            reason,
            extra: Extra::new(),
        })
    }

    pub fn step_start(turn: u64, step: u64) -> Self {
        Self::StepStart(Step {
            turn,
            step,
            extra: Extra::new(),
        })
    }

    pub fn step_end(turn: u64, step: u64) -> Self {
        Self::StepEnd(Step {
            turn,
            step,
            extra: Extra::new(),
        })
    }

    /// The `tool/call` event for a call the model made, logged as the tool
    /// starts.
    pub fn tool_call(turn: u64, step: u64, call: &karaka_model::ToolCall) -> Self {
        Self::ToolCall(ToolCallData {
            turn,
            step,
            call_id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            extra: Extra::new(),
        })
    }

    /// A `request/header` logged for `reason`.
    pub fn request_header(header: RequestHeader, reason: RequestHeaderReason) -> Self {
        Self::RequestHeader(RequestHeaderData {
            header,
            reason,
            starts_series: false,
            extra: Extra::new(),
        })
    }

    /// An `assistant/attempt` with an empty stream, for tests and for a
    /// call that sent nothing.
    pub fn assistant_attempt(turn: u64, step: u64) -> Self {
        Self::AssistantAttempt(AssistantAttemptData {
            turn,
            step,
            stream: Vec::new(),
            extra: Extra::new(),
        })
    }
}

/// How a surface event joins the model-visible surface, and which earlier
/// events it came from. (dsh v4: `SurfaceIntent`, the envelope's
/// `surfaceOp` and `sourceEventSeqs`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceIntent {
    pub op: SurfaceOp,
    /// The earlier events this one was made from, such as the `tool/call` a
    /// result answers, or the nodes a compaction summary replaces. Empty
    /// means the envelope has no `sourceEventSeqs`; v4 never writes an
    /// empty list.
    pub sources: Vec<Seq>,
}

impl SurfaceIntent {
    /// Added to the end of the surface, citing nothing.
    pub fn append() -> Self {
        Self {
            op: SurfaceOp::Append,
            sources: Vec::new(),
        }
    }
}

/// dsh v4: `SurfaceOp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceOp {
    /// Added to the end of the surface. (dsh v4: `"append"`.)
    Append,
    /// Takes the place of the surface nodes from `start` to `end`, both
    /// included, which leave the surface. Compaction uses it to shorten a
    /// history without deleting events, and the system prompt uses it to
    /// rewrite node 0. (dsh v4: `{ op: "replace", startSeq, endSeq }`.)
    Replace { start: Seq, end: Seq },
}

/// An event of a type Karaka doesn't model, kept exactly as it was read.
///
/// `surface_op` and `source_event_seqs` are kept as raw JSON too: v4 lets an
/// unknown event marked `ignorable` carry any value there, and they never
/// affect Karaka's surface.
#[derive(Clone, Debug, PartialEq)]
pub struct Passthrough {
    /// The v4 type, such as `"approval/asked"`.
    pub event_type: String,
    pub data: Value,
    pub surface_op: Option<Value>,
    pub source_event_seqs: Option<Value>,
}

/// An event's position in its session's log.
///
/// The first event is 0 and each append adds 1, with no gaps, so an event's
/// seq is also its index in the log. Play This Life's stores rely on that:
/// usage records are unique per seq, and follow rejects a gap.
/// (dsh v4: `SessionSeq`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(pub u64);

impl Seq {
    /// The seq after this one.
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// When an event was appended, in milliseconds since the Unix epoch.
/// (dsh v4: the envelope's `time`.)
///
/// Signed, because v4 accepts any safe integer here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Time(pub i64);

impl Time {
    /// The current time. A clock set before 1970 reads as 0.
    pub fn now() -> Self {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);
        Self(i64::try_from(millis).unwrap_or(i64::MAX))
    }
}

/// One event in the log, with its envelope. (dsh v4: `SessionEvent`.)
///
/// Its JSON form is v4's envelope: `type`, `seq`, `time`, `data`, and the
/// optional `surfaceOp`, `sourceEventSeqs` and `ignorable`.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Equal to the entry's index in its log.
    pub seq: Seq,
    pub time: Time,
    /// A reader that doesn't know this event's type may skip it.
    /// (dsh v4: `ignorable: true`, written only on plugin records.)
    pub ignorable: bool,
    pub event: SessionEvent,
}
