//! [`AnthropicPlugin`], which puts an [`AnthropicModel`] in a Karaka runtime
//! as its `dyn Model` port.

use std::fmt;
use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_model::Model;
use serde::Deserialize;

use crate::AnthropicModel;

/// How the plugin reaches Anthropic.
///
/// Plugins will be configured from a YAML file read at startup, and each
/// plugin's part of that file will be read into its own typed config like
/// this one. That is why it derives `Deserialize`: a missing or misspelled
/// field is then reported when the file is read, before anything starts.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicConfig {
    pub api_key: String,
    /// Another server, such as a proxy. `None` means Anthropic's own.
    pub base_url: Option<String>,
    /// The `max_tokens` for requests that don't set their own. `None` keeps
    /// the model's default.
    pub default_max_tokens: Option<u32>,
}

/// Leaves out the API key, so a config can be logged safely.
impl fmt::Debug for AnthropicConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicConfig")
            .field("base_url", &self.base_url)
            .field("default_max_tokens", &self.default_max_tokens)
            .finish_non_exhaustive()
    }
}

/// Provides the `dyn Model` port, backed by Anthropic's Messages API.
#[derive(Debug)]
pub struct AnthropicPlugin {
    config: AnthropicConfig,
}

impl AnthropicPlugin {
    pub fn new(config: AnthropicConfig) -> Self {
        Self { config }
    }
}

impl Plugin for AnthropicPlugin {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<dyn Model>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        // Every call would be refused without a key. Failing here means a
        // missing key shows up when the runtime starts, not on the first call.
        if self.config.api_key.trim().is_empty() {
            return Err("the Anthropic API key is empty".into());
        }

        let mut model = AnthropicModel::new(self.config.api_key.clone());
        if let Some(url) = &self.config.base_url {
            model = model.with_base_url(url.clone());
        }
        if let Some(max_tokens) = self.config.default_max_tokens {
            model = model.with_default_max_tokens(max_tokens);
        }
        ctx.provide::<dyn Model>(Arc::new(model));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::AnthropicConfig;

    #[test]
    fn the_config_reads_with_only_the_key_given() {
        let config: AnthropicConfig =
            serde_json::from_str(r#"{"api_key": "sk-secret"}"#).expect("the config reads");
        assert_eq!(config.api_key, "sk-secret");
        assert_eq!(config.base_url, None);
        assert_eq!(config.default_max_tokens, None);
    }

    #[test]
    fn a_misspelled_field_is_rejected() {
        let error = serde_json::from_str::<AnthropicConfig>(r#"{"api_key": "k", "base_ulr": "x"}"#)
            .expect_err("the config is rejected");
        assert!(error.to_string().contains("base_ulr"), "{error}");
    }

    #[test]
    fn debug_output_leaves_out_the_api_key() {
        let config = AnthropicConfig {
            api_key: "sk-secret".into(),
            base_url: None,
            default_max_tokens: None,
        };
        assert!(!format!("{config:?}").contains("sk-secret"));
    }
}
