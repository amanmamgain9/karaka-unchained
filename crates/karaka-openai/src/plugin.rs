//! [`OpenAiPlugin`], which registers an [`OpenAiModel`] in a Karaka
//! runtime's [`ModelRegistry`].

use std::fmt;
use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_model::{Effort, ModelRegistry};
use serde::Deserialize;

use crate::{MaxTokensField, OpenAiModel};

/// How the plugin reaches an OpenAI-compatible server.
///
/// Plugins will be configured from a YAML file read at startup, and each
/// plugin's part of that file will be read into its own typed config like
/// this one. That is why it derives `Deserialize`: a missing or misspelled
/// field is then reported when the file is read, before anything starts.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiConfig {
    /// The name the model is registered under, and the plugin's name, such
    /// as "openai", "deepseek" or "ollama". Required: this one crate serves
    /// all of those servers, so no single default would fit.
    pub route: String,
    /// The part before `/chat/completions`, such as
    /// `https://api.openai.com/v1`. See [`OpenAiModel`] for more.
    pub base_url: String,
    /// `None` for a local server that needs no key.
    pub api_key: Option<String>,
    /// The effort for requests that don't set their own, sent as
    /// `reasoning_effort`. `None` sends none.
    pub default_effort: Option<Effort>,
    /// DeepSeek's thinking switch: `true` sends `{"type": "enabled"}`,
    /// `false` sends `{"type": "disabled"}`. `None` sends nothing, which is
    /// right for every other server.
    pub thinking: Option<bool>,
    /// The field `max_tokens` goes out as. `max_tokens` unless set.
    #[serde(default)]
    pub max_tokens_field: MaxTokensField,
}

/// Leaves out the API key, so a config can be logged safely.
impl fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field("route", &self.route)
            .field("base_url", &self.base_url)
            .field("has_api_key", &self.api_key.is_some())
            .field("default_effort", &self.default_effort)
            .field("thinking", &self.thinking)
            .field("max_tokens_field", &self.max_tokens_field)
            .finish_non_exhaustive()
    }
}

/// Registers a model backed by an OpenAI-compatible server in the
/// [`ModelRegistry`], under the config's route.
///
/// The plugin is named after its route, so DeepSeek and Ollama can each have
/// one in the same runtime. Two with the same route are stopped by the
/// kernel before either starts, because their names clash.
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
        &self.config.route
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<ModelRegistry>()]
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
        // DeepSeek has no effort for thinking that is off, so the pair can
        // never make a request that works. It is caught here, at startup.
        if self.config.thinking == Some(false)
            && let Some(effort) = self.config.default_effort
        {
            return Err(
                format!("the default effort is {effort:?}, but thinking is turned off").into(),
            );
        }
        if let Some(effort) = self.config.default_effort {
            model = model.with_default_effort(effort);
        }
        if let Some(on) = self.config.thinking {
            model = model.with_thinking(on);
        }
        model = model.with_max_tokens_field(self.config.max_tokens_field);
        // An empty or taken route fails here. Its error becomes this
        // plugin's setup error, which the runtime reports by plugin name.
        ctx.get::<ModelRegistry>()
            .register(self.config.route.clone(), Arc::new(model))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use karaka_model::Effort;

    use super::OpenAiConfig;
    use crate::MaxTokensField;

    #[test]
    fn the_config_reads_without_a_key() {
        let config: OpenAiConfig =
            serde_json::from_str(r#"{"route": "ollama", "base_url": "http://localhost:11434/v1"}"#)
                .expect("the config reads");
        assert_eq!(config.route, "ollama");
        assert_eq!(config.base_url, "http://localhost:11434/v1");
        assert_eq!(config.api_key, None);
        assert_eq!(config.default_effort, None);
        assert_eq!(config.thinking, None);
        assert_eq!(config.max_tokens_field, MaxTokensField::MaxTokens);
    }

    #[test]
    fn the_config_reads_effort_thinking_and_the_max_tokens_field() {
        let config: OpenAiConfig = serde_json::from_str(
            r#"{"route": "deepseek", "base_url": "https://api.deepseek.com", "default_effort": "max", "thinking": true, "max_tokens_field": "max_completion_tokens"}"#,
        )
        .expect("the config reads");
        assert_eq!(config.default_effort, Some(Effort::Max));
        assert_eq!(config.thinking, Some(true));
        assert_eq!(config.max_tokens_field, MaxTokensField::MaxCompletionTokens);
    }

    #[test]
    fn the_config_needs_a_route() {
        let error = serde_json::from_str::<OpenAiConfig>(r#"{"base_url": "http://x"}"#)
            .expect_err("the config is rejected");
        assert!(error.to_string().contains("route"), "{error}");
    }

    #[test]
    fn a_misspelled_field_is_rejected() {
        let error = serde_json::from_str::<OpenAiConfig>(
            r#"{"route": "x", "base_url": "http://x", "apikey": "k"}"#,
        )
        .expect_err("the config is rejected");
        assert!(error.to_string().contains("apikey"), "{error}");
    }

    #[test]
    fn debug_output_leaves_out_the_api_key() {
        let config = OpenAiConfig {
            route: "openai".into(),
            base_url: "http://localhost".into(),
            api_key: Some("sk-secret".into()),
            default_effort: None,
            thinking: None,
            max_tokens_field: MaxTokensField::MaxTokens,
        };
        assert!(!format!("{config:?}").contains("sk-secret"));
    }
}
