//! A model provider for Anthropic's Messages API.
//!
//! [`AnthropicModel`] turns `karaka_model` requests into Anthropic's JSON,
//! sends them over HTTP, and turns Anthropic's streamed events back into
//! `karaka_model` events.
//!
//! Each stage has its own module, so each can be tested on its own: the
//! request body, reading server-sent events out of the HTTP body, and
//! decoding those events.

mod decoder;
mod model;
mod request;
mod sse;

pub use model::AnthropicModel;
