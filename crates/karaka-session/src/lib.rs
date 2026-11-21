//! The session seam: what a session is, and the store that holds them.
//!
//! A session is an append-only log of [`SessionEvent`]s, and the log is the
//! single source of truth. The history the model sees is derived from it by
//! [`SessionLog::history`], never stored on its own. Keeping only the log is
//! what makes replay, compaction and reading dsh's stored logs possible:
//! each of them is a different way of reading the same events.
//!
//! # dsh v4
//!
//! The events follow dsh's session format v4 (dsh-session 0.2.1-rc.2),
//! which Play This Life stores verbatim as JSON. An [`Entry`] reads and
//! writes v4's event envelope exactly, and every event type Karaka doesn't
//! model is kept whole as a [`Passthrough`], so a stored log keeps every
//! event and every seq. Each part names the v4 code it mirrors.
//!
//! - `event.rs`, `data.rs`, `message.rs`: the event vocabulary.
//! - `json.rs`: the JSON envelope.
//! - `surface.rs`: which events the model sees, in what order.
//! - `derive.rs`: the history, and which system messages become the prompt.
//! - `convert.rs`: between v4's messages and `karaka-model`'s.
//! - `repair.rs`: the events that close a turn a crash left open.
//! - `request.rs`: the request header, the route's context and failed
//!   attempts, which the loop logs around each model call.
//!
//! # Storage
//!
//! This crate keeps sessions in memory only, in a [`SessionStore`] that
//! [`SessionsPlugin`] provides as a port. Durable backends such as Postgres
//! will be their own plugins, saving these same events. They can rely on
//! two things: events are only ever appended, and their seqs are contiguous
//! from 0. A backend follows a session with [`Session::events_since`] and
//! loads one with [`SessionStore::restore`].

mod convert;
mod data;
mod derive;
mod event;
mod json;
mod log;
mod message;
mod plugin;
mod repair;
mod request;
mod store;
mod surface;

pub use data::{
    AssistantMessageData, CancelCause, DeveloperMessageData, Failure, Step, SystemMessageData,
    ToolCallData, ToolError, ToolResultData, TurnEnd, TurnEndReason, TurnStart,
};
pub use derive::History;
pub use event::{Entry, Passthrough, Seq, SessionEvent, SurfaceIntent, SurfaceOp, Time};
pub use log::{LogError, SessionLog};
pub use message::{
    AssistantMessage, AssistantRole, ContentBlock, ContextSection, DeveloperMessage, DeveloperRole,
    Extra, ModelKind, ModelSource, PromptSource, ReasoningBlock, RuntimeContextSource,
    SystemMessage, SystemPromptKind, SystemPromptSource, SystemRole, TextBlock, TokenUsage,
    ToolCallBlock, ToolKind, ToolResultMessage, ToolRole, ToolSource, UserMessage, UserRole,
    UserSource,
};
pub use plugin::SessionsPlugin;
pub use repair::{
    CloseCause, FORKED_NOT_STARTED, FORKED_STARTED, INTERRUPTED_NOT_STARTED, INTERRUPTED_STARTED,
    TOOL_NOT_STARTED, TOOL_OUTCOME_UNKNOWN, ToolCallRecovery, interrupted_turn_closers,
    open_turn_closers,
};
pub use request::{
    AdapterDefaults, AssistantAttemptData, CallConfig, IN_HISTORY, RequestContext, RequestHeader,
    RequestHeaderData, RequestHeaderReason, call_config_equals, canonical_header, header_equals,
};
pub use store::{Session, SessionError, SessionId, SessionStore};
