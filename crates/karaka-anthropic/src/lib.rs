//! A model provider for Anthropic's Messages API.
//!
//! [`AnthropicModel`] turns `karaka_model` requests into Anthropic's JSON,
//! sends them over HTTP, and turns Anthropic's streamed events back into
//! `karaka_model` events. [`AnthropicPlugin`] builds one from an
//! [`AnthropicConfig`] and provides it to a Karaka runtime as `dyn Model`.
//!
//! Each stage has its own module, so each can be tested on its own: the
//! request body, and decoding the streamed events. Reading those events out
//! of the HTTP body is shared with other providers, in `karaka-sse`.

mod decoder;
mod model;
mod plugin;
mod request;

pub use model::AnthropicModel;
pub use plugin::{AnthropicConfig, AnthropicPlugin};
