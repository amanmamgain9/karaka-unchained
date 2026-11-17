//! The model seam: the provider-neutral shape of a conversation, and the
//! [`Model`] port every model provider implements.
//!
//! Provider crates translate these types to and from their own wire format.
//! Nothing outside a provider crate ever sees that format.

mod message;
mod model;

pub use message::{Block, Message, Role, ToolCall, ToolResult};
pub use model::{
    ErrorKind, Event, EventStream, Model, ModelError, Request, Response, StopReason, ToolSchema,
    Usage, complete,
};
