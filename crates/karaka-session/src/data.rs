//! The `data` of each event type Karaka models, as dsh v4 writes it.
//! (dsh v4: `SessionEventMap` in `packages/core/session/src/types.ts`.)
//!
//! Like the messages in `message.rs`, each struct keeps the fields it
//! doesn't name in `extra`, so a v4 log writes back unchanged.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::event::Seq;
use crate::message::{
    AssistantMessage, DeveloperMessage, Extra, SystemMessage, TokenUsage, ToolResultMessage,
    UserMessage, present,
};

/// dsh v4: `system/message`, `{ turn, step, message }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemMessageData {
    pub turn: u64,
    pub step: u64,
    pub message: SystemMessage,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh v4: `developer/message`, `{ turn, step, message, headerSeq? }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeveloperMessageData {
    pub turn: u64,
    pub step: u64,
    pub message: DeveloperMessage,
    /// The earlier `request/header` that defines every tool this message
    /// adds. Present exactly when it adds tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_seq: Option<Seq>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh v4: `assistant/message`, `{ turn, step, message, stream, usage?,
/// interrupted? }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessageData {
    pub turn: u64,
    pub step: u64,
    pub message: AssistantMessage,
    /// The exact timed stream the model sent, in v4's compact records, as
    /// raw JSON, so a record a dsh adapter wrote keeps every field. See
    /// `stream.rs` for the records and [`expand_stream`](crate::expand_stream)
    /// for reading them.
    pub stream: Vec<Value>,
    /// What the call cost. Absent when the provider reported nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    /// The turn was cancelled while this message streamed, and this is the
    /// part that arrived. v4 writes it only as `true`.
    #[serde(
        default,
        skip_serializing_if = "is_false",
        deserialize_with = "only_true"
    )]
    pub interrupted: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh v4: `tool/result`, `{ turn, step, message, error?, meta? }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultData {
    pub turn: u64,
    pub step: u64,
    pub message: ToolResultMessage,
    /// Why the tool failed, outside what the model reads. v4 allows it only
    /// when `message.isError` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
    /// The tool's own data for showing the result, such as a diff. Any JSON.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub meta: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh v4: `tool/result`'s `error: { name, code, reason? }`.
///
/// Play This Life's client shows its upgrade prompt when `code` is
/// `pro_required`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolError {
    pub name: String,
    pub code: String,
    /// The raw reason, for the user rather than the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh v4: `turn/start`, `{ turn }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TurnStart {
    pub turn: u64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh v4: `turn/end`, `{ turn, reason }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TurnEnd {
    pub turn: u64,
    pub reason: TurnEndReason,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh v4: `step/start` and `step/end`, both `{ turn, step }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub turn: u64,
    pub step: u64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh v4: `tool/call`, `{ turn, step, callId, name, arguments }`.
///
/// Logged when a tool starts running. Repair uses it to tell a call whose
/// outcome is unknown from one that never started.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallData {
    pub turn: u64,
    pub step: u64,
    pub call_id: String,
    pub name: String,
    /// The arguments exactly as the model wrote them, unparsed.
    pub arguments: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// One of an agent's two lists of pending input. (dsh: `InboxTarget`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InboxTarget {
    /// Prompts waiting for a turn each.
    #[serde(rename = "next-turn")]
    NextTurn,
    /// Input waiting for the next step boundary.
    #[serde(rename = "next-step")]
    NextStep,
}

/// Why a splice removed messages, when they were dropped rather than
/// taken. (dsh: `outcome`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpliceOutcome {
    Canceled,
}

/// dsh v4: `agent/inbox/spliced`, `{ target, start, removedCount?,
/// inserted, outcome? }`: one change to an agent's durable pending input,
/// a JavaScript `splice` of one list. (dsh: `agent/src/types.ts`.)
///
/// Its messages are ordinary user messages, as v4 checks them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxSplice {
    pub target: InboxTarget,
    pub start: u64,
    /// Written only when something was removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_count: Option<u64>,
    pub inserted: Vec<UserMessage>,
    /// `canceled` when the removed messages were dropped, as a cancel drops
    /// them; absent when they were taken, as a step takes its input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<SpliceOutcome>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Why a turn ended. (dsh v4: `TurnEndReason`, keyed by `kind`.)
///
/// Plugins may add kinds, so an unknown one is kept in `Other`. So is a
/// known one with fields Karaka doesn't model: see [`exact`].
#[derive(Clone, Debug, PartialEq)]
pub enum TurnEndReason {
    /// dsh v4: `{ kind: "completed" }`.
    Completed,
    /// A cancellation interrupted the live turn. (dsh v4: `aborted`.)
    Aborted(CancelCause),
    /// A plugin refused the turn. (dsh v4: `blocked`.)
    Blocked,
    /// The turn failed. (dsh v4: `{ kind: "error", error }`.)
    Error(Failure),
    /// A step hit its output limit. (dsh v4: `max-tokens`.)
    MaxTokens,
    /// Repair closed a turn a crash left open. (dsh v4: `interrupted`.)
    Interrupted,
    /// A fork closed a turn that was open at its cut. (dsh v4: `forked`.)
    Forked,
    /// Any other reason, as it was.
    Other(Value),
}

/// Why a live turn was cancelled. (dsh v4: `TurnEndCancelCause`.)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CancelCause {
    User,
    Parent,
    Hook {
        reason: String,
    },
    Disposed,
    /// An imported turn whose record said only that it was cancelled.
    Legacy,
}

/// A failure from the provider or transport. (dsh v4: `LlmFailure`.)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub message: String,
    /// A stable machine-readable code, such as `UNKNOWN`.
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// How long the provider asked to wait, in milliseconds. A JSON number
    /// as written, not an integer: dsh's DeepSeek transport turns
    /// `Retry-After: 1.2345` into `1234.5` (`llm-deepseek/src/transport.ts:37`),
    /// and its v4 validator takes any finite positive number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_retry_after_ms: Option<serde_json::Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offload_images: Option<u64>,
}

/// The reasons Karaka models, in v4's JSON shape. [`TurnEndReason`] reads
/// through this, falling back to `Other`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum KnownReason {
    Completed,
    Aborted { reason: CancelCause },
    Blocked,
    Error { error: Failure },
    MaxTokens,
    Interrupted,
    Forked,
}

impl Serialize for TurnEndReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let known = match self {
            Self::Completed => KnownReason::Completed,
            Self::Aborted(reason) => KnownReason::Aborted {
                reason: reason.clone(),
            },
            Self::Blocked => KnownReason::Blocked,
            Self::Error(error) => KnownReason::Error {
                error: error.clone(),
            },
            Self::MaxTokens => KnownReason::MaxTokens,
            Self::Interrupted => KnownReason::Interrupted,
            Self::Forked => KnownReason::Forked,
            Self::Other(value) => return value.serialize(serializer),
        };
        known.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TurnEndReason {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Ok(match exact::<KnownReason>(&value) {
            Some(KnownReason::Completed) => Self::Completed,
            Some(KnownReason::Aborted { reason }) => Self::Aborted(reason),
            Some(KnownReason::Blocked) => Self::Blocked,
            Some(KnownReason::Error { error }) => Self::Error(error),
            Some(KnownReason::MaxTokens) => Self::MaxTokens,
            Some(KnownReason::Interrupted) => Self::Interrupted,
            Some(KnownReason::Forked) => Self::Forked,
            None => Self::Other(value),
        })
    }
}

/// `value` read as `T`, but only if writing that `T` gives `value` back.
///
/// A typed enum can silently drop a field it doesn't name, such as an extra
/// field on `{ kind: "completed" }`. Checking the round trip catches that,
/// and the caller then keeps the raw value instead.
fn exact<T: Serialize + DeserializeOwned>(value: &Value) -> Option<T> {
    let typed = T::deserialize(value).ok()?;
    let written = serde_json::to_value(&typed).ok()?;
    (written == *value).then_some(typed)
}

pub(crate) fn is_false(value: &bool) -> bool {
    !value
}

/// Reads a v4 marker field, which is either absent or `true`.
pub(crate) fn only_true<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    if bool::deserialize(deserializer)? {
        Ok(true)
    } else {
        Err(serde::de::Error::custom(
            "a marker field must be absent or true",
        ))
    }
}
