//! The model seam: the provider-neutral shape of a conversation, the
//! [`Model`] trait every model provider implements, and the
//! [`ModelRegistry`] port where providers register their models by route.
//!
//! Provider crates translate these types to and from their own wire format.
//! Nothing outside a provider crate ever sees that format.

mod message;
mod model;
mod plugin;
mod registry;
mod retry;
mod retry_after;
mod wording;

pub use message::{Block, Message, Origin, Replay, Role, ToolCall, ToolResult};
pub use model::{
    Effort, ErrorKind, Event, EventStream, Model, ModelError, Refusal, Request, Response,
    StopReason, ToolSchema, Usage, complete,
};
pub use plugin::ModelsPlugin;
pub use registry::{ModelRegistry, RegistryError};
pub use retry::{
    Backoff, BackoffConfig, DEFAULT_RETRYABLE_CODES, MAX_DELAY_MS, RetryMode, RetryPolicy,
    RetryPolicyConfig, RetryPolicyError,
};
// Helpers for provider crates, which all read failures the same way.
pub use retry_after::parse_retry_after;
pub use wording::{is_context_overflow, is_out_of_quota};
