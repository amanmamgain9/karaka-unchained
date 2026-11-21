//! [`Profile`]: whose version of the Messages API a model speaks.

use serde::Deserialize;

/// Whose version of the Messages API a model speaks.
///
/// DeepSeek serves Anthropic's format too, at
/// `https://api.deepseek.com/anthropic`, and dsh v4 talks to DeepSeek that
/// way (`llm-deepseek/src/config.ts:115`). The streamed events are the same,
/// but the request differs in enough places that each profile writes its
/// own body. The module docs of `deepseek.rs` list the differences.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Anthropic's own API.
    #[default]
    Anthropic,
    /// DeepSeek's Messages API, sent as dsh v4's `llm-deepseek` sends it.
    DeepSeek,
}

impl Profile {
    /// The provider's name, for error messages.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::DeepSeek => "DeepSeek",
        }
    }
}
