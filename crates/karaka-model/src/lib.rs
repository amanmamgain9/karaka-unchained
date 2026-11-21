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

pub use message::{Block, Message, Role, ToolCall, ToolResult};
pub use model::{
    Effort, ErrorKind, Event, EventStream, Model, ModelError, Request, Response, StopReason,
    ToolSchema, Usage, complete,
};
pub use plugin::ModelsPlugin;
pub use registry::{ModelRegistry, RegistryError};
