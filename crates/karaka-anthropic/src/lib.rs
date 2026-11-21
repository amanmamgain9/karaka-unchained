//! A model provider for Anthropic's Messages API, and for DeepSeek's copy of
//! it.
//!
//! [`AnthropicModel`] turns `karaka_model` requests into the Messages API's
//! JSON, sends them over HTTP, and turns the streamed events back into
//! `karaka_model` events. [`AnthropicPlugin`] builds one from an
//! [`AnthropicConfig`] and registers it in a Karaka runtime's
//! `ModelRegistry`, under the config's route.
//!
//! DeepSeek serves the same API, and dsh v4 talks to DeepSeek through it. A
//! [`Profile`] says which of the two a model talks to. The DeepSeek profile
//! sends what dsh's `llm-deepseek` sends, which differs in many details.
//!
//! Each stage has its own module, so each can be tested on its own: the
//! request body, one for each profile, and decoding the streamed events.
//! Reading those events out of the HTTP body is shared with other
//! providers, in `karaka-sse`.

mod decoder;
mod deepseek;
mod model;
mod plugin;
mod profile;
mod request;

pub use deepseek::DeepSeekThinking;
pub use model::AnthropicModel;
pub use plugin::{AnthropicConfig, AnthropicPlugin};
pub use profile::Profile;
pub use request::{CacheMode, ThinkingDisplay};
