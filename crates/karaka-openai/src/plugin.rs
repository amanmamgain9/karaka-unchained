//! [`OpenAiPlugin`], which puts an [`OpenAiModel`] in a Karaka runtime as
//! its `dyn Model` port.

use std::fmt;
use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_model::Model;
use serde::Deserialize;

use crate::OpenAiModel;

/// How the plugin reaches an OpenAI-compatible server.
///
/// Plugins will be configured from a YAML file read at startup, and each
/// plugin's part of that file will be read into its own typed config like
/// this one. That is why it derives `Deserialize`: a missing or misspelled
/// field is then reported when the file is read, before anything starts.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiConfig {
    /// The part before `/chat/completions`, such as
    /// `https://api.openai.com/v1`. See [`OpenAiModel`] for more.
    pub base_url: String,
    /// `None` for a local server that needs no key.
    pub api_key: Option<String>,
}

/// Leaves out the API key, so a config can be logged safely.
impl fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field("base_url", &self.base_url)
            .field("has_api_key", &self.api_key.is_some())
            .finish_non_exhaustive()
    }
}

/// Provides the `dyn Model` port, backed by an OpenAI-compatible server.
#[derive(Debug)]
pub struct OpenAiPlugin {
    config: OpenAiConfig,
}

impl OpenAiPlugin {
    pub fn new(config: OpenAiConfig) -> Self {
        Self { config }
    }
}

impl Plugin for OpenAiPlugin {
    fn name(&self) -> &str {
        "openai"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<dyn Model>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        // Every call would fail without a server to send it to. Failing here
        // means that shows up when the runtime starts, not on the first call.
        if self.config.base_url.trim().is_empty() {
            return Err("the OpenAI-compatible base URL is empty".into());
        }

        let mut model = OpenAiModel::new(self.config.base_url.clone());
        if let Some(key) = &self.config.api_key {
            model = model.with_api_key(key.clone());
        }
        ctx.provide::<dyn Model>(Arc::new(model));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::OpenAiConfig;

    #[test]
    fn the_config_reads_without_a_key() {
        let config: OpenAiConfig =
            serde_json::from_str(r#"{"base_url": "http://localhost:11434/v1"}"#)
                .expect("the config reads");
        assert_eq!(config.base_url, "http://localhost:11434/v1");
        assert_eq!(config.api_key, None);
    }

    #[test]
    fn a_misspelled_field_is_rejected() {
        let error =
            serde_json::from_str::<OpenAiConfig>(r#"{"base_url": "http://x", "apikey": "k"}"#)
                .expect_err("the config is rejected");
        assert!(error.to_string().contains("apikey"), "{error}");
    }

    #[test]
    fn debug_output_leaves_out_the_api_key() {
        let config = OpenAiConfig {
            base_url: "http://localhost".into(),
            api_key: Some("sk-secret".into()),
        };
        assert!(!format!("{config:?}").contains("sk-secret"));
    }
}
