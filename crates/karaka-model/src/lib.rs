//! The model seam: the provider-neutral shape of a conversation, and (next)
//! the port every model provider implements.
//!
//! Provider crates translate these types to and from their own wire format.
//! Nothing outside a provider crate ever sees that format.

mod message;

pub use message::{Block, Message, Role, ToolCall, ToolResult};
