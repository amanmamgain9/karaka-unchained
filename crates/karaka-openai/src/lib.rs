//! A model provider for OpenAI-compatible Chat Completions APIs.
//!
//! Many servers speak the same `POST /chat/completions` format: OpenAI
//! itself, DeepSeek, gateways such as OpenRouter, and local servers such as
//! llama.cpp, vLLM and Ollama. [`OpenAiModel`] serves all of them; only the
//! base URL and the API key change. [`OpenAiPlugin`] builds one from an
//! [`OpenAiConfig`] and registers it in a Karaka runtime's `ModelRegistry`,
//! under the config's route. One runtime can hold several, each with its
//! own route.
//!
//! Each stage has its own module, so each can be tested on its own: the
//! request body, and decoding the streamed chunks. Reading those chunks out
//! of the HTTP body is shared with other providers, in `karaka-sse`.

mod decoder;
mod model;
mod plugin;
mod request;

pub use model::OpenAiModel;
pub use plugin::{OpenAiConfig, OpenAiPlugin};
pub use request::MaxTokensField;
